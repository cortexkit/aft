use std::cell::Cell;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use super::manifest_acceptance::{resolve_with_basis, DecisionBasis};
use super::{
    current_platform, delegate_after_invalid_manifest_notice, enable_gate, gh_shim_state_dir_from,
    is_unbound_safe, refuse, regressed_disposition, trust_root, unix_seconds, ManifestProblem,
    ManifestResolution, RefusalCode, RegressedDisposition, StatePaths, NO_TRUST_ROOT_REFUSAL,
};

pub(super) struct ManifestGuard<'a> {
    paths: &'a StatePaths,
    initial: ManifestResolution,
    basis: DecisionBasis,
    basis_changed: Cell<bool>,
}

impl<'a> ManifestGuard<'a> {
    pub(super) fn new(paths: &'a StatePaths) -> Self {
        let (initial, basis) = resolve_with_basis(paths, unix_seconds());
        Self {
            paths,
            initial,
            basis,
            basis_changed: Cell::new(false),
        }
    }

    pub(super) fn initial(&self) -> &ManifestResolution {
        &self.initial
    }

    pub(super) fn observe(&self, now: u64) -> ManifestResolution {
        let (current, basis) = resolve_with_basis(self.paths, now);
        if self.basis != basis
            || std::mem::discriminant(&self.initial) != std::mem::discriminant(&current)
        {
            self.basis_changed.set(true);
        }
        current
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
        let local_provisioned = manifest_state_was_provisioned(&self.paths.root);
        let governed_launch = enable_gate::launched_enabled();
        if local_provisioned || governed_launch {
            let pin = trust_root::read(self.paths);
            if matches!(pin, Err(ManifestProblem::NoTrustRoot)) || (governed_launch && pin.is_err())
            {
                return Some(refuse(
                    RefusalCode::ManifestUnavailable,
                    NO_TRUST_ROOT_REFUSAL,
                ));
            }
        }
        if governed_launch
            || local_provisioned
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
        let current = self.observe(unix_seconds());
        if self.basis_changed.get() {
            return refuse(
                RefusalCode::ManifestUnavailable,
                "GitHub routing policy changed before the command could run. Start the command again.",
            );
        }
        if !is_unbound_safe(args) {
            match current {
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
        "gh-routing-manifest.json",
        trust_root::FILE_NAME,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_shim_a_list_or_pin_alone_is_provisioning_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().to_owned());
        assert!(!manifest_state_was_provisioned(temp.path()));
        for artifact in [&paths.manifest, &paths.trust_root] {
            std::fs::write(artifact, b"unverified artifact").unwrap();
            assert!(
                manifest_state_was_provisioned(temp.path()),
                "{}",
                artifact.display()
            );
            std::fs::remove_file(artifact).unwrap();
        }
    }

    #[test]
    fn gh_shim_refuses_an_active_basis_change_even_if_the_first_list_returns() {
        let temp = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(temp.path().to_owned());
        let now = unix_seconds();
        let mut list = super::super::tests::fixture_manifest();
        super::super::tests::write_signed_manifest(&paths, list.clone(), now);
        let original = std::fs::read(&paths.manifest).unwrap();
        let guard = ManifestGuard::new(&paths);
        assert!(matches!(guard.initial(), ManifestResolution::Active(_)));
        list.issued_at_unix_secs = now;
        super::super::tests::write_signed_manifest(&paths, list, now);
        assert_ne!(std::fs::read(&paths.manifest).unwrap(), original);
        assert!(matches!(guard.observe(now), ManifestResolution::Active(_)));
        std::fs::write(&paths.manifest, original).unwrap();
        let args = [OsString::from("issue"), OsString::from("list")];
        assert_eq!(
            guard.delegate(&args, |_| panic!(
                "the command used a different signed list"
            )),
            86
        );
    }
}
