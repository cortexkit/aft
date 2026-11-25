//! Tool versions are independent of the legacy catalog's semantics counters.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use cortexkit_role_tool_provider::{
    call::{SchemaPin, ToolCallRequest, ToolMajorPin, VersionedSchemaPin},
    catalog::{CatalogTool, ToolVersion},
    errors,
};
use serde_json::json;
use subc_protocol::ErrorBody;

use super::{legacy_file_path_digests, served_tools, CallerRole, CatalogPreset, ServedTool};

// Runners must read versioned schema pins (tp2) before AFT advertises tool versions.
const PUBLISH_TOOL_VERSIONS: bool = true;
const INITIAL: ToolVersion = ToolVersion::new(1, 0, 0);
const VERSIONS: &[(&str, ToolVersion)] = &[
    ("apply_patch", INITIAL),
    ("ast_replace", INITIAL),
    ("ast_search", INITIAL),
    ("bash", ToolVersion::new(2, 0, 0)),
    ("bash_kill", INITIAL),
    ("bash_status", INITIAL),
    ("bash_watch", INITIAL),
    ("bash_write", INITIAL),
    ("callgraph", INITIAL),
    ("conflicts", INITIAL),
    ("delete", INITIAL),
    ("edit", INITIAL),
    ("glob", INITIAL),
    ("grep", INITIAL),
    ("import", INITIAL),
    ("inspect", INITIAL),
    ("move", INITIAL),
    ("outline", INITIAL),
    ("powershell", INITIAL),
    ("read", INITIAL),
    ("safety", INITIAL),
    ("search", INITIAL),
    ("status", INITIAL),
    ("write", INITIAL),
    ("zoom", INITIAL),
];

// This history is frozen independently of today's schemas and version table.
// A version bump must not silently remap a legacy caller to the new major.
static SHIPPED: LazyLock<BTreeMap<String, u64>> = LazyLock::new(|| {
    serde_json::from_str(include_str!("shipped_pins.json")).expect("shipped tool pin history")
});

pub(in crate::subc) fn shipped_major(pin: &str) -> Option<u64> {
    SHIPPED.get(pin).copied()
}

/// Current schemas across presets, including schemas offered only on some routes.
pub(in crate::subc) fn current_pin_tools() -> impl Iterator<Item = &'static CatalogTool> {
    let head = [
        &super::RUNNER_HEAD_TOOLS.hashline_edit,
        &super::RUNNER_HEAD_TOOLS.bash_watch,
    ];
    CatalogPreset::ALL
        .into_iter()
        .flat_map(served_tools)
        .chain(head)
        .flat_map(|tool| {
            [
                Some(&tool.catalog),
                tool.with_ref.as_ref(),
                tool.with_runon.as_ref(),
            ]
            .into_iter()
            .flatten()
        })
}

#[cfg(feature = "test-hooks")]
static VERSION_PROBE: LazyLock<Option<bool>> =
    LazyLock::new(
        || match std::env::var("AFT_TEST_CONFORMANCE_TOOL_VERSIONS").as_deref() {
            Ok("1") => Some(true),
            Ok("0") => Some(false),
            _ => None,
        },
    );

pub(super) fn version(name: &str) -> ToolVersion {
    let version = VERSIONS
        .iter()
        .find(|(tool, _)| *tool == name)
        .expect("every served tool has a version")
        .1;
    #[cfg(feature = "test-hooks")]
    if *VERSION_PROBE == Some(true) && name == "read" {
        return ToolVersion::new(version.major, version.minor + 1, version.patch);
    }
    version
}

pub(super) fn prepare_catalog(tools: &mut [CatalogTool]) {
    #[cfg(feature = "test-hooks")]
    let publish = VERSION_PROBE.unwrap_or(PUBLISH_TOOL_VERSIONS);
    #[cfg(not(feature = "test-hooks"))]
    let publish = PUBLISH_TOOL_VERSIONS;
    #[cfg(test)]
    let publish = CATALOG_PUBLICATION
        .with(|value| value.get())
        .unwrap_or(publish);
    if !publish {
        for tool in tools {
            tool.version = None;
        }
    }
}

#[cfg(test)]
thread_local! {
    static CATALOG_PUBLICATION: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Render archived catalog fixtures in their original wire format even after
/// production catalogs start advertising versions.
#[cfg(test)]
pub(super) fn legacy_catalog() -> impl Drop {
    struct Restore(Option<bool>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CATALOG_PUBLICATION.with(|value| value.set(self.0));
        }
    }
    Restore(CATALOG_PUBLICATION.with(|value| value.replace(Some(false))))
}

pub(super) fn admit_pin(
    call: &ToolCallRequest,
    encoded: &str,
    role: CallerRole,
    served: &ServedTool,
    validator_tool: &CatalogTool,
    hashline_edit: bool,
) -> Result<(), ErrorBody> {
    let pin = VersionedSchemaPin::parse(encoded)
        .map_err(|error| errors::invalid_request("schema_pin", error.to_string()))?;
    let head_watch = (role == CallerRole::Head && call.name == "bash_watch")
        .then(|| &super::RUNNER_HEAD_TOOLS.bash_watch.catalog);
    let preferred = if role.is_worker() || hashline_edit {
        &served.catalog
    } else {
        served_tools(CatalogPreset::Head)
            .iter()
            .find(|entry| entry.catalog.name == call.name)
            .map_or(head_watch.unwrap_or(&served.catalog), |entry| {
                &entry.catalog
            })
    };
    let major = match &pin {
        VersionedSchemaPin::Tp2(pin) => pin.major,
        VersionedSchemaPin::Tp1(pin) => {
            // The head-only watch grammar is not a worker tool, and the
            // hashline edit grammar is only available on a hashline route.
            let other_grammar = (head_watch.is_none()
                && call.name == "bash_watch"
                && pin.schema_digest == super::RUNNER_HEAD_TOOLS.bash_watch.catalog.schema_digest)
                || (!hashline_edit
                    && call.name == "edit"
                    && pin.schema_digest
                        == super::RUNNER_HEAD_TOOLS.hashline_edit.catalog.schema_digest);
            match shipped_major(encoded).filter(|_| !other_grammar) {
                Some(major) => major,
                None => {
                    return Err(legacy_refusal(
                        pin,
                        preferred,
                        head_watch,
                        served,
                        validator_tool,
                    ))
                }
            }
        }
        _ => {
            return Err(errors::invalid_request(
                "schema_pin",
                "unknown schema pin scheme",
            ))
        }
    };
    let current = [
        head_watch,
        Some(preferred),
        Some(&served.catalog),
        served.with_runon.as_ref(),
        served.with_ref.as_ref(),
    ]
    .into_iter()
    .flatten()
    .find(|tool| tool.schema_digest == digest(&pin))
    .unwrap_or(preferred);
    if major != current.version.expect("served tool has a version").major {
        return Err(errors::tool_major_changed(
            &call.name,
            encoded,
            &current.schema_pin().encode().expect("current pin"),
        ));
    }
    Ok(())
}

fn legacy_refusal(
    pin: &SchemaPin,
    preferred: &CatalogTool,
    head_watch: Option<&CatalogTool>,
    served: &ServedTool,
    validator_tool: &CatalogTool,
) -> ErrorBody {
    let head_watch = head_watch.filter(|entry| entry.schema_digest == pin.schema_digest);
    if pin.schema_digest == validator_tool.schema_digest
        || pin.schema_digest == served.catalog.schema_digest
        || legacy_file_path_digests(&pin.tool).contains(&pin.schema_digest)
        || head_watch.is_some()
    {
        let semantics = head_watch.map_or_else(
            || {
                if pin.tool == "bash_watch" {
                    validator_tool.semantics
                } else {
                    preferred.semantics
                }
            },
            |entry| entry.semantics,
        );
        if pin.semantics != semantics {
            return ErrorBody::new(errors::TOOL_SEMANTICS_CHANGED, "semantics pin is stale")
                .with_detail(
                    json!({"tool": pin.tool, "expected": pin.semantics, "current": semantics}),
                );
        }
    }
    ErrorBody::new(errors::TOOL_SCHEMA_CHANGED, "schema pin is stale").with_detail(
        json!({"tool": pin.tool, "expected": pin.schema_digest, "current": validator_tool.schema_digest}),
    )
}

fn digest(pin: &VersionedSchemaPin) -> &str {
    match pin {
        VersionedSchemaPin::Tp1(pin) => &pin.schema_digest,
        VersionedSchemaPin::Tp2(pin) => &pin.schema_digest,
        _ => unreachable!("unrecognized versioned pin"),
    }
}

pub(super) fn takes_hashline_arm(call: &ToolCallRequest) -> bool {
    match call
        .schema_pin
        .as_deref()
        .and_then(|pin| VersionedSchemaPin::parse(pin).ok())
    {
        Some(VersionedSchemaPin::Tp1(pin)) => {
            pin.schema_digest == super::RUNNER_HEAD_TOOLS.hashline_edit.catalog.schema_digest
        }
        // A tp2 caller can have a digest from another minor. Its arguments
        // still identify which of the two edit grammars it uses.
        _ => call.arguments.get("patch").is_some(),
    }
}

pub(super) fn rename_pin(encoded: &str, from: &str, to: &str) -> Option<String> {
    let pin = VersionedSchemaPin::parse(encoded).ok()?;
    if pin.tool() != from {
        return None;
    }
    match pin {
        VersionedSchemaPin::Tp1(pin) => SchemaPin::new(to, pin.schema_digest, pin.semantics)
            .encode()
            .ok(),
        VersionedSchemaPin::Tp2(pin) => ToolMajorPin::new(to, pin.schema_digest, pin.major)
            .encode()
            .ok(),
        _ => None,
    }
}

mod routed;
pub(in crate::subc) use routed::{admit_routed_call, record_pin, route_call};

#[cfg(test)]
mod tests;
