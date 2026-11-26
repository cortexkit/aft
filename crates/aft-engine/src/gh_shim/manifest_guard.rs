use std::ffi::OsString;
use std::io;
use std::path::Path;

use super::{
    current_platform, delegate_after_invalid_manifest_notice, gh_shim_state_dir_from,
    is_unbound_safe, refuse, regressed_disposition, resolve_manifest, unix_seconds,
    ManifestProblem, ManifestResolution, RefusalCode, RegressedDisposition, StatePaths,
};

pub(super) struct ManifestGuard<'a> {
    paths: &'a StatePaths,
}

impl<'a> ManifestGuard<'a> {
    pub(super) fn new(paths: &'a StatePaths) -> Self {
        Self { paths }
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
