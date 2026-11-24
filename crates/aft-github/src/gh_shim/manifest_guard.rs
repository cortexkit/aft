use std::cell::OnceCell;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use subc_client_rs::{ConsumerOptions, SubcConsumer};

use super::{
    connection_file_from_config_doc, current_platform, delegate_after_invalid_manifest_notice,
    gh_shim_state_dir_from, is_unbound_safe, read_user_config_doc, refuse, regressed_disposition,
    relay_client, resolve_manifest, unix_seconds, ManifestProblem, ManifestResolution, RefusalCode,
    RegressedDisposition, StatePaths, DISCOVERY_BUDGET,
};

pub(super) struct ManifestGuard<'a> {
    paths: &'a StatePaths,
    // Dispatch and the final upstream guard share one lookup for this invocation.
    routing_may_be_offered: OnceCell<bool>,
}

impl<'a> ManifestGuard<'a> {
    pub(super) fn new(paths: &'a StatePaths) -> Self {
        Self {
            paths,
            routing_may_be_offered: OnceCell::new(),
        }
    }

    pub(super) fn dispatch_without_valid_manifest(
        &self,
        args: &[OsString],
        problem: Option<&ManifestProblem>,
        delegate: impl FnOnce(&[OsString]) -> i32,
    ) -> i32 {
        if let Some(status) = self.unavailable_refusal(args) {
            return status;
        }
        match problem {
            Some(problem) => delegate_after_invalid_manifest_notice(args, problem, delegate),
            None => delegate(args),
        }
    }

    pub(super) fn unavailable_refusal(&self, args: &[OsString]) -> Option<i32> {
        if is_unbound_safe(args) {
            return None;
        }
        // An agent may select an empty state directory. Check the default too,
        // without constructing StatePaths there: that constructor tightens permissions.
        let default_root = gh_shim_state_dir_from(
            None,
            crate::environment::non_empty_os_var("XDG_STATE_HOME").as_deref(),
            crate::environment::non_empty_os_var("HOME").as_deref(),
        );
        if manifest_state_was_provisioned(&self.paths.root)
            || manifest_state_was_provisioned(&default_root)
            || *self
                .routing_may_be_offered
                .get_or_init(routing_may_be_offered)
        {
            return Some(refuse(
                RefusalCode::ManifestUnavailable,
                "GitHub writes are refused because this machine's signed routing manifest is missing or invalid; without it the write would run as the operator's own account. Ask the operator to repair the manifest. Reads still work.",
            ));
        }
        None
    }

    pub(super) fn delegate(
        &self,
        args: &[OsString],
        delegate: impl FnOnce(&[OsString]) -> i32,
    ) -> i32 {
        if !is_unbound_safe(args) {
            match resolve_manifest(self.paths, unix_seconds()) {
                ManifestResolution::Invalid(_) | ManifestResolution::Dormant => {
                    if let Some(status) = self.unavailable_refusal(args) {
                        return status;
                    }
                }
                ManifestResolution::Regressed { manifest, problem } => {
                    if let RegressedDisposition::Refuse { code, text } =
                        regressed_disposition(args, &manifest, current_platform(), &problem)
                    {
                        return refuse(code, &text);
                    }
                }
                ManifestResolution::Active(_) => {}
            }
        }
        delegate(args)
    }
}

fn manifest_state_was_provisioned(root: &Path) -> bool {
    [
        "last-valid-manifest.json",
        "manifest-version-high-water.json",
    ]
    .iter()
    .any(|name| match std::fs::symlink_metadata(root.join(name)) {
        Ok(_) => true,
        // An unreadable record cannot establish that the machine was never
        // provisioned. Only absence permits that fallback.
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    })
}

fn routing_may_be_offered() -> bool {
    // Use config only to locate the daemon, not to decide what it serves.
    // A configured but unusable connection does not establish absence.
    let Some(connection_file) = read_user_config_doc()
        .as_deref()
        .and_then(connection_file_from_config_doc)
    else {
        return false;
    };
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    else {
        return true;
    };
    let offered = runtime.block_on(async {
        tokio::time::timeout(DISCOVERY_BUDGET, async {
            let options = ConsumerOptions {
                call_timeout: DISCOVERY_BUDGET,
                handshake_timeout: DISCOVERY_BUDGET,
                ..ConsumerOptions::default()
            };
            let consumer = SubcConsumer::connect(&connection_file, options)
                .await
                .ok()?;
            let catalog = consumer.catalog_list().await.ok()?;
            Some(relay_client::relay_holder(&catalog.modules).is_some())
        })
        .await
    });
    // Only a successful catalog response can rule out the write relay.
    !matches!(offered, Ok(Some(false)))
}
