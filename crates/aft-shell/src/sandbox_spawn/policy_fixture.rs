use super::{SandboxHost, SandboxMode};
use crate::{
    access_policy::AccessPolicy, config::Config, pinned_config::PinnedConfig, protocol::Response,
};
use std::{path::PathBuf, sync::Arc};

pub(crate) struct SandboxFixture {
    config: parking_lot::RwLock<Arc<Config>>,
    _temporary_storage: tempfile::TempDir,
    #[cfg(unix)]
    grants: Arc<super::EscalationGrantState>,
}

impl SandboxFixture {
    pub(crate) fn new(mut config: Config) -> Self {
        let storage = tempfile::tempdir().unwrap();
        config
            .storage_dir
            .get_or_insert_with(|| storage.path().to_path_buf());
        Self {
            config: parking_lot::RwLock::new(Arc::new(config)),
            _temporary_storage: storage,
            #[cfg(unix)]
            grants: Arc::default(),
        }
    }

    pub(super) fn strict(project_root: PathBuf) -> Self {
        Self::new(Config {
            project_root: Some(project_root),
            sandbox: crate::config::SandboxConfig {
                profile: crate::config::SandboxRoles {
                    head: SandboxMode::Strict,
                    worker: SandboxMode::Strict,
                },
                ..Default::default()
            },
            ..Default::default()
        })
    }

    pub(crate) fn update_config(&self, update: impl FnOnce(&mut Config)) {
        update(Arc::make_mut(&mut self.config.write()));
    }
}

impl PinnedConfig for SandboxFixture {
    fn config(&self) -> Arc<Config> {
        self.config.read().clone()
    }
}

impl SandboxHost for SandboxFixture {
    fn access_policy(&self, _: &str) -> Result<AccessPolicy, Response> {
        let config = self.config();
        let home = crate::production_storage::account_home().unwrap();
        Ok(AccessPolicy::from_config(
            &config,
            &self.storage_dir(),
            config.project_root.iter().cloned().collect(),
            &home,
        ))
    }

    fn storage_dir(&self) -> PathBuf {
        self.config.read().storage_dir.clone().unwrap()
    }

    #[cfg(unix)]
    fn escalation_grants(&self) -> &Arc<super::EscalationGrantState> {
        &self.grants
    }

    #[cfg(target_os = "linux")]
    fn pin_config(&self) -> Box<dyn super::ConfigPin + '_> {
        panic!("the local sandbox fixture does not prepare remote shells")
    }
}
