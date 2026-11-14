use super::ConfigTier;
use parking_lot::RwLock;
use std::{fmt, sync::Arc};

/// Connection-scoped user settings. A project bind or reload may take a
/// snapshot of this tier, but must not read the local aft.jsonc file instead.
#[derive(Clone)]
pub struct ApprovedUser(Arc<RwLock<ConfigTier>>);

impl ApprovedUser {
    pub fn new(tier: ConfigTier) -> Self {
        Self(Arc::new(RwLock::new(tier)))
    }

    pub fn replace(&self, tier: ConfigTier) {
        *self.0.write() = tier;
    }

    pub fn snapshot(&self) -> ConfigTier {
        self.0.read().clone()
    }
}

impl fmt::Debug for ApprovedUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovedUser(..)")
    }
}
