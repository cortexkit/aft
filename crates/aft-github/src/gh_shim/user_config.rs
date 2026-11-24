use super::*;

pub fn configured_connection_file() -> Option<PathBuf> {
    connection_file_from_user_config(&crate::subc_config::cortexkit_user_config_path()?)
}

#[cfg(test)]
pub(super) fn configured_connection_file_from(
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<PathBuf> {
    connection_file_from_user_config(&crate::subc_config::user_config_path_from(
        xdg_config_home,
        home,
    )?)
}

fn connection_file_from_user_config(config_path: &Path) -> Option<PathBuf> {
    // This reader becomes `subc_transport::connection_file::discover(explicit)`
    // when the transport API reaches AFT. That call replaces these ordered rungs:
    // explicit (exclusive); non-empty SUBC_CONNECTION_FILE (exclusive); non-empty
    // XDG_RUNTIME_DIR/subc-connection.json; non-empty
    // HOME/.local/share/cortexkit/run/subc-connection.json; user-scoped temp file.
    // Last re-derived 2026-09-06 against subconscious
    // d5e09914b0791a66f2a5a00a9bb3422860ade95e: compare `(rung, guard)` pairs with
    // `subc-transport/src/connection_file.rs::discovery_candidates_with_environment`
    // and resolve `CONNECTION_FILE_NAME` and `PROD_CONNECTION_RELATIVE_PATH`.
    // Until the call lands here, only trusted user config can provide the explicit
    // path; invalid or unreadable paths resolve to `None` for the rung classifier.
    let doc = fs::read_to_string(config_path).ok()?;
    connection_file_from_config_doc(&doc).filter(|path| path.is_file())
}

/// Read the raw user-tier `aft.jsonc` document for the shim's config gates.
/// `None` means the config file was absent or unreadable, which the rung
/// determination treats as "no user config" (structural rungs decide).
pub(super) fn read_user_config_doc() -> Option<String> {
    let config_path = crate::subc_config::cortexkit_user_config_path()?;
    fs::read_to_string(config_path).ok()
}

/// Read the effective `github.enabled && github.shim` gate from user config.
/// The deprecated `gh_shim.enabled` alias remains a fallback for one minor.
pub(super) fn gh_shim_enabled_from_config_doc(doc: &str) -> Option<bool> {
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(doc)).ok()?;
    let github = value.get("github");
    let master = github
        .and_then(|config| config.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let shim = github
        .and_then(|config| config.get("shim"))
        .and_then(Value::as_bool)
        .or_else(|| {
            value
                .get("gh_shim")
                .and_then(|config| config.get("enabled"))
                .and_then(Value::as_bool)
        })
        .unwrap_or(true);
    Some(master && shim)
}

pub(super) fn connection_file_from_config_doc(doc: &str) -> Option<PathBuf> {
    let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(doc)).ok()?;
    let raw = value.get("subc")?.get("connection_file")?.as_str()?.trim();
    let path = PathBuf::from(raw);
    (!raw.is_empty() && path.is_absolute()).then_some(path)
}
