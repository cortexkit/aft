use super::ConfigTier;
use parking_lot::RwLock;
use std::{fmt, sync::Arc};

/// Daemon-managed effective settings. A registered project's slot may fall
/// back to the connection's user view while the project is unknown.
#[derive(Clone)]
pub struct ApprovedUser(Arc<Approved>);

enum Approved {
    User(RwLock<ConfigTier>),
    Project(RwLock<Option<ConfigTier>>, ApprovedUser),
}

impl ApprovedUser {
    pub fn new(tier: ConfigTier) -> Self {
        Self(Arc::new(Approved::User(RwLock::new(tier))))
    }

    pub fn project(user: &Self) -> Self {
        Self(Arc::new(Approved::Project(RwLock::new(None), user.clone())))
    }

    pub fn replace(&self, tier: ConfigTier) {
        match self.0.as_ref() {
            Approved::User(current) => *current.write() = tier,
            Approved::Project(current, _) => *current.write() = Some(tier),
        }
    }

    pub fn use_user_view(&self) {
        if let Approved::Project(current, _) = self.0.as_ref() {
            *current.write() = None;
        }
    }

    pub fn snapshot(&self) -> ConfigTier {
        match self.0.as_ref() {
            Approved::User(tier) => tier.read().clone(),
            Approved::Project(tier, user) => tier.read().clone().unwrap_or_else(|| user.snapshot()),
        }
    }

    pub fn uses(&self, changed: &Self) -> bool {
        Arc::ptr_eq(&self.0, &changed.0)
            || matches!(self.0.as_ref(), Approved::Project(tier, user)
                if tier.read().is_none() && user.uses(changed))
    }
}

impl fmt::Debug for ApprovedUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovedUser(..)")
    }
}
