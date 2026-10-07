//! Remote-run policy. Whether a call runs remotely is the caller's own choice
//! (the bash `runon` argument); this policy only says whether a session may
//! make that choice, and which runner demand fills in when the call names none.
use serde::{Deserialize, Serialize};

/// The runner demands `runon` accepts today.
pub const KNOWN_DEMANDS: &[&str] = &["linux"];

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteExecPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// The demand a `runon` call without specifics runs under. It never makes
    /// a call remote by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_demand: Option<String>,
    /// Older worker plans carried a list of command prefixes that were sent
    /// to the remote runner automatically. Remote runs are now chosen per call,
    /// so the list is read (to keep accepting those plans) and never used.
    #[serde(default, rename = "commands", skip_serializing)]
    pub legacy_commands: Option<serde_json::Value>,
}

/// Resolve the demand a `runon` call asks for. An empty value takes the
/// session's default demand; any other value must be a known demand, and an
/// unknown one is refused by name rather than guessed.
pub fn resolve_demand(runon: &str, default_demand: Option<&str>) -> Result<String, String> {
    let requested = runon.trim();
    let demand = if requested.is_empty() {
        default_demand
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .ok_or_else(|| {
                format!(
                "runon needs a runner demand and this session sets no default; use runon: \"{}\"",
                KNOWN_DEMANDS[0]
            )
            })?
    } else {
        requested
    };
    if KNOWN_DEMANDS.contains(&demand) {
        Ok(demand.to_owned())
    } else {
        Err(unknown_demand(demand))
    }
}

/// The refusal for a demand no runner serves.
pub fn unknown_demand(demand: &str) -> String {
    format!(
        "unknown runner demand {demand:?}; runon accepts {}",
        KNOWN_DEMANDS
            .iter()
            .map(|d| format!("{d:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_shapes_old_and_new_decode_and_commands_are_ignored() {
        let new: RemoteExecPolicy =
            serde_json::from_value(serde_json::json!({"enabled": true, "default_demand": "linux"}))
                .unwrap();
        assert!(new.enabled);
        assert_eq!(new.default_demand.as_deref(), Some("linux"));
        let old: RemoteExecPolicy = serde_json::from_value(
            serde_json::json!({"enabled": true, "commands": ["cargo test", "not a valid prefix!"]}),
        )
        .unwrap();
        assert!(old.enabled);
        assert!(old.legacy_commands.is_some());
        // A re-serialized policy never carries the ignored list forward.
        assert_eq!(
            serde_json::to_value(&old).unwrap(),
            serde_json::json!({"enabled": true})
        );
        assert!(serde_json::from_value::<RemoteExecPolicy>(
            serde_json::json!({"enabled": true, "unexpected": 1})
        )
        .is_err());
    }

    #[test]
    fn demand_resolution_names_unknown_demands_and_uses_the_default_only_when_unspecified() {
        assert_eq!(resolve_demand("linux", None).unwrap(), "linux");
        assert_eq!(resolve_demand("", Some("linux")).unwrap(), "linux");
        assert!(resolve_demand("", None)
            .unwrap_err()
            .contains("runon needs a runner demand"));
        let error = resolve_demand("windows", Some("linux")).unwrap_err();
        assert!(
            error.contains("unknown runner demand \"windows\""),
            "{error}"
        );
        let error = resolve_demand("", Some("gpu")).unwrap_err();
        assert!(error.contains("unknown runner demand \"gpu\""), "{error}");
    }
}
