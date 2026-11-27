use super::*;
use sha2::{Digest, Sha256};

#[derive(Default, Eq, PartialEq)]
pub(super) struct DecisionBasis {
    manifest_digest: Option<[u8; 32]>,
    pin: Option<(u64, trust_root::Pin)>,
}

pub(super) fn load_verified_manifest(
    paths: &StatePaths,
    now: u64,
) -> Result<VerifiedManifest, ManifestProblem> {
    accept(paths, now, None)
}

fn accept(
    paths: &StatePaths,
    now: u64,
    basis: Option<&mut DecisionBasis>,
) -> Result<VerifiedManifest, ManifestProblem> {
    if !paths.root.exists() {
        return Err(ManifestProblem::Missing);
    }
    let _acceptance_lock = manifest_acceptance_lock(paths)
        .map_err(|_| ManifestProblem::Invalid("manifest acceptance lock unavailable".into()))?;
    let bytes = fs::read(&paths.manifest);
    let root = trust_root::read_snapshot(paths);
    if let Some(basis) = basis {
        basis.manifest_digest = bytes
            .as_ref()
            .ok()
            .map(|bytes| Sha256::digest(bytes).into());
        basis.pin = root
            .as_ref()
            .ok()
            .map(|root| (root.generation, root.pin.clone()));
    }
    let bytes = bytes.map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => ManifestProblem::Missing,
        _ => ManifestProblem::Invalid("manifest unreadable".into()),
    })?;
    let pin = root?.pin;
    let envelope: SignedManifest = serde_json::from_slice(&bytes)
        .map_err(|error| ManifestProblem::Invalid(error.to_string()))?;
    let verified = validate_manifest_candidate(paths, &envelope, now, &pin)?;
    let manifest = &verified.manifest;
    if manifest.manifest_version > version_high_water(paths) {
        write_version_high_water(paths, manifest.manifest_version);
    }
    write_last_valid_manifest(paths, manifest, &bytes);
    retain_manifest(paths, &envelope, manifest.manifest_version);
    Ok(verified)
}

// The daemon publisher and short-lived shim processes share this OS lock so a
// newer acceptance cannot race a publisher's high-water check and rename.
pub(super) fn manifest_acceptance_lock(
    paths: &StatePaths,
) -> io::Result<crate::fs_lock::AdvisoryLockGuard> {
    crate::private_storage::open_root(&paths.root)?;
    let file = crate::private_storage::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.root.join("manifest-acceptance.lock"))?;
    file.lock()?;
    Ok(crate::fs_lock::AdvisoryLockGuard::from_locked_file(file))
}

pub(super) fn resolve_manifest(paths: &StatePaths, now: u64) -> ManifestResolution {
    resolve(paths, load_manifest(paths, now))
}

pub(super) fn resolve_with_basis(
    paths: &StatePaths,
    now: u64,
) -> (ManifestResolution, DecisionBasis) {
    let mut basis = DecisionBasis::default();
    let result = accept(paths, now, Some(&mut basis)).map(|verified| verified.manifest);
    (resolve(paths, result), basis)
}

fn resolve(paths: &StatePaths, result: Result<Manifest, ManifestProblem>) -> ManifestResolution {
    match result {
        Ok(manifest) => ManifestResolution::Active(manifest),
        Err(ManifestProblem::Missing | ManifestProblem::NoTrustRoot) => ManifestResolution::Dormant,
        Err(problem) => match read_last_valid_manifest(paths) {
            Some(cache) => ManifestResolution::Regressed {
                manifest: cache.manifest,
                problem,
            },
            None => ManifestResolution::Invalid(problem),
        },
    }
}
