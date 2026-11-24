//! The shim's operator on/off gate.
//!
//! Two sources can say whether the shim is on. The daemon resolves the full
//! configuration for the harness that launched the child, including any
//! `harnesses.<name>.github.shim` override, and puts the shim on the child's
//! `PATH` only when that resolved value is on; it then marks the child with
//! `AFT_GH_SHIM_ENABLED=1`. Without that marker (a shim started outside an AFT
//! child), the shim falls back to reading the raw user file, which only knows
//! the top-level keys.
//!
//! The marker can only turn the shim on. The child's own shell can set any
//! variable, so a value other than `1` is ignored rather than treated as "off":
//! nothing an agent writes into its environment can switch governance off.

use std::ffi::OsStr;

use serde_json::Value;

/// True when the shim must behave as operator-disabled (byte-transparent
/// pass-through to the real `gh`).
pub(super) fn shim_switched_off(config_doc: Option<&str>) -> bool {
    switched_off(config_doc, launch_marker().as_deref())
}

fn launch_marker() -> Option<std::ffi::OsString> {
    // Unit tests often run inside an AFT-governed shell, which carries the
    // marker. Only the fresh-process test that sets the marker on purpose
    // opts in, so the other tests keep exercising the file gate.
    #[cfg(test)]
    std::env::var_os(TEST_HONOUR_MARKER_ENV)?;
    std::env::var_os(crate::gh_shim_paths::GH_SHIM_ENABLED_ENV)
}

#[cfg(test)]
pub(super) const TEST_HONOUR_MARKER_ENV: &str = "AFT_TEST_HONOUR_GH_SHIM_ENABLED";

fn switched_off(config_doc: Option<&str>, marker: Option<&OsStr>) -> bool {
    if marker == Some(OsStr::new("1")) {
        return false;
    }
    gh_shim_enabled_from_config_doc(config_doc.unwrap_or("")) == Some(false)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Base off, OpenCode override on: the daemon resolved "on" for an
    /// OpenCode child and put the shim on its PATH.
    const HARNESS_ON_OVER_BASE_OFF: &str =
        r#"{"github":{"shim":false},"harnesses":{"opencode":{"github":{"shim":true}}}}"#;

    #[test]
    fn launch_marker_overrides_a_top_level_off_the_harness_turned_on() {
        assert!(!switched_off(
            Some(HARNESS_ON_OVER_BASE_OFF),
            Some(OsStr::new("1"))
        ));
    }

    #[test]
    fn no_marker_value_can_switch_an_enabled_shim_off() {
        let enabled = r#"{"github":{"shim":true}}"#;
        for marker in ["0", "", "false", "no"] {
            assert!(
                !switched_off(Some(enabled), Some(OsStr::new(marker))),
                "marker {marker:?} must not disable the shim"
            );
        }
        assert!(!switched_off(None, Some(OsStr::new("0"))));
    }

    #[test]
    fn without_the_marker_the_file_decides_as_before() {
        assert!(switched_off(Some(HARNESS_ON_OVER_BASE_OFF), None));
        assert!(switched_off(
            Some(HARNESS_ON_OVER_BASE_OFF),
            Some(OsStr::new("0"))
        ));
        assert!(!switched_off(Some(r#"{"github":{"shim":true}}"#), None));
        assert!(!switched_off(None, None));
    }
}
