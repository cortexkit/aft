use super::*;

mod api;
mod secret;

pub(super) use api::{classify_api, POLICY_GLOB, PUT_GLOB};
pub(super) use secret::{classify_secret, targets_secret, SECRET_SET};

const VERSION: u64 = 30;

pub(super) fn dispatch(
    args: &[OsString],
    paths: &StatePaths,
    cwd: &Path,
    now: u64,
    delegate: impl FnOnce(&[OsString]) -> i32,
) -> Option<i32> {
    if targets_secret(args) {
        return Some(secret::dispatch(
            args,
            paths,
            cwd,
            now,
            secret::stdin_ready,
            delegate,
        ));
    }
    if secret::targets_environment_delete(args) {
        let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
            return Some(manifest_required());
        };
        if !matches!(
            super::classify(args, &manifest, current_platform()),
            Classification::Admin { .. } | Classification::Governed { .. }
        ) {
            return Some(refuse(
                RefusalCode::Unclassified,
                "environment secret deletion is not declared in the signed manifest",
            ));
        }
    }
    api::dispatch(args, paths, now, delegate)
}

fn declared(args: &[OsString], paths: &StatePaths, now: u64) -> Result<(Manifest, String), i32> {
    let ManifestResolution::Active(manifest) = resolve_manifest(paths, now) else {
        return Err(manifest_required());
    };
    match super::classify(args, &manifest, current_platform()) {
        Classification::Admin { tuple } => Ok((manifest, tuple)),
        _ => Err(undeclared(args, manifest.manifest_version)),
    }
}

fn manifest_required() -> i32 {
    refuse(
        RefusalCode::ManifestUnavailable,
        "deployment environment writes require a valid signed routing manifest",
    )
}

fn undeclared(args: &[OsString], version: u64) -> i32 {
    refuse(
        RefusalCode::Unclassified,
        &unclassified_refusal_text(args, version),
    )
}

fn operator_allowed(manifest: &Manifest, tuple: &str) -> Result<(), i32> {
    if !operator_bypass_requested() {
        return Err(refuse(
            RefusalCode::AdminTier,
            &admin_tier_refusal_text(tuple),
        ));
    }
    if let Some(status) = refuse_bypass_bot_credentials(&manifest.detectors) {
        return Err(status);
    }
    native_guard::permission_from_process()
        .map_err(|text| refuse(RefusalCode::WorkerNativeGh, &text))
}

fn valid_environment(name: &str) -> bool {
    // Keep the encoded name in one literal URL segment, with no gh placeholders.
    !name.trim().is_empty()
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\', '%', '?', '#', '{', '}'])
        && !name.chars().any(char::is_control)
}

#[cfg(test)]
mod tests;
