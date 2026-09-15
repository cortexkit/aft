//! Resolver input projections. Digests describe consumed fields, never file bytes.
use std::collections::BTreeMap;

pub(crate) type FactDigests = BTreeMap<String, String>;

pub(crate) fn project(path: &[u8], bytes: &[u8]) -> Option<FactDigests> {
    let mut facts = BTreeMap::new();
    let mut insert = |name: &str, value: serde_json::Value| {
        facts.insert(
            name.to_owned(),
            blake3::hash(&serde_json::to_vec(&value).expect("JSON value"))
                .to_hex()
                .to_string(),
        );
    };
    match path.rsplit(|b| *b == b'/').next()? {
        b"package.json" => {
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
            for field in ["name", "exports", "module", "main"] {
                insert(field, value.get(field).cloned().unwrap_or_default());
            }
            insert(
                "workspaces",
                serde_json::json!(crate::callgraph::workspace_patterns(&value)),
            );
        }
        b"pnpm-workspace.yaml" => insert(
            "packages",
            serde_json::json!(crate::callgraph::parse_pnpm_workspace_patterns(bytes)),
        ),
        b"tsconfig.json" => {
            // The resolver reads only the nearest config; it does not follow extends.
            let value: serde_json::Value = serde_json::from_slice(bytes).unwrap_or_default();
            let options = &value["compilerOptions"];
            insert("compilerOptions.paths", options["paths"].clone());
            insert("baseUrl", options["baseUrl"].clone());
        }
        b"Cargo.toml" => {
            let (name, lib_name) = crate::callgraph_store::rust_manifest_name_fields(bytes);
            insert("manifest.name", serde_json::json!(name));
            insert("manifest.lib.name", serde_json::json!(lib_name));
            // The disk resolver uses TOML, unlike the manifest crate-name reader.
            // Keep its inputs distinct: workspace members do not constrain the
            // manifest resolver's project-wide crate map.
            let value = std::str::from_utf8(bytes)
                .ok()
                .and_then(|s| toml::from_str::<toml::Value>(s).ok());
            for (section, field) in [
                ("package", "name"),
                ("lib", "name"),
                ("lib", "path"),
                ("workspace", "members"),
            ] {
                insert(
                    &format!("disk.{section}.{field}"),
                    serde_json::to_value(
                        value
                            .as_ref()
                            .and_then(|v| v.get(section))
                            .and_then(|v| v.get(field)),
                    )
                    .expect("TOML value"),
                );
            }
        }
        _ => return None,
    }
    Some(facts)
}

#[derive(Default)]
pub(super) struct InputDiff {
    pub changed: std::collections::BTreeSet<(String, String)>,
    pub inputs_changed: bool,
    pub unknown: usize,
}

/// Project each changed input once per side of the transition. Resolver reads
/// persist identities, not config bytes or recomputed per-reference digests.
pub(super) fn diff_inputs(
    base: &crate::views::Manifest,
    next: &crate::views::Manifest,
    changed: &std::collections::BTreeSet<Vec<u8>>,
    connection: &rusqlite::Connection,
) -> crate::callgraph_store::Result<InputDiff> {
    use crate::callgraph_store::join::{CallgraphBlob, ManifestBlobReader};
    use crate::views::{ManifestEntry, RelPath};
    let reader = super::ManifestViewBlobReader::new(connection);
    let mut result = InputDiff::default();
    for path in changed {
        let rel = RelPath::new(path.clone()).expect("manifest path");
        let marked = [base, next].iter().any(|manifest| {
            matches!(
                manifest.get(&rel),
                Some(ManifestEntry::Regular {
                    resolution_input: true,
                    ..
                })
            )
        });
        if !marked && !crate::callgraph_store::join::view_resolution_config(path) {
            continue;
        }
        let key = |manifest: &crate::views::Manifest| match manifest.get(&rel) {
            Some(ManifestEntry::Regular { planes, .. }) => planes.callgraph.clone(),
            _ => None,
        };
        if key(base) == key(next) && base.get(&rel).is_some() == next.get(&rel).is_some() {
            continue;
        }
        result.inputs_changed = true;
        let mut projected = Vec::new();
        for manifest in [base, next] {
            let bytes = if manifest.get(&rel).is_none() {
                Some(Vec::new())
            } else if let Some(key) = key(manifest) {
                let payload = reader
                    .read_callgraph_blob(&key)
                    .map_err(|e| {
                        crate::callgraph_store::CallGraphStoreError::Unavailable(e.to_string())
                    })?
                    .ok_or_else(|| {
                        crate::callgraph_store::CallGraphStoreError::Unavailable(format!(
                            "missing resolution input blob {key}"
                        ))
                    })?;
                match CallgraphBlob::from_bytes(&payload).map_err(|e| {
                    crate::callgraph_store::CallGraphStoreError::Unavailable(e.to_string())
                })? {
                    CallgraphBlob::Config(config) => Some(config.source),
                    _ => None,
                }
            } else {
                None
            };
            projected.push(bytes.and_then(|bytes| project(path, &bytes)));
        }
        let (Some(before), Some(after)) = (&projected[0], &projected[1]) else {
            result.unknown += 1;
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            result.unknown += 1;
            continue;
        };
        for name in before.keys().chain(after.keys()) {
            if before.get(name) != after.get(name) {
                result.changed.insert((path.into(), name.clone()));
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed(path: &str, before: &str, after: &str) -> Vec<String> {
        let before = project(path.as_bytes(), before.as_bytes()).unwrap();
        project(path.as_bytes(), after.as_bytes())
            .unwrap()
            .into_iter()
            .filter(|(name, digest)| before.get(name) != Some(digest))
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn package_version_and_types_are_not_resolution_facts() {
        assert!(changed(
            "package.json",
            r#"{"name":"p","version":"1","types":"a"}"#,
            r#"{"name":"p","version":"2","types":"b"}"#
        )
        .is_empty());
    }

    #[test]
    fn each_json_read_field_changes_one_digest() {
        for field in ["name", "exports", "module", "main", "workspaces"] {
            let value = if field == "workspaces" {
                serde_json::json!(["packages/*"])
            } else {
                serde_json::json!("new")
            };
            let after = serde_json::json!({field: value}).to_string();
            assert_eq!(changed("package.json", "{}", &after), [field]);
        }
        assert_eq!(
            changed(
                "tsconfig.json",
                "{}",
                r#"{"compilerOptions":{"paths":{"x":["a"]}}}"#
            ),
            ["compilerOptions.paths"]
        );
        assert_eq!(
            changed(
                "tsconfig.json",
                "{}",
                r#"{"compilerOptions":{"baseUrl":"src"}}"#
            ),
            ["baseUrl"]
        );
        assert!(changed("tsconfig.json", "{}", r#"{"extends":"other.json"}"#).is_empty());
        assert!(changed(
            "package.json",
            r#"{"workspaces":["p/*"]}"#,
            r#"{"workspaces":{"packages":["p/*"]}}"#
        )
        .is_empty());
    }

    #[test]
    fn line_parsers_match_resolver_semantics() {
        assert_eq!(
            changed(
                "pnpm-workspace.yaml",
                "packages:\n - 'one'",
                "packages:\n - 'two'"
            ),
            ["packages"]
        );
        assert!(changed(
            "pnpm-workspace.yaml",
            "packages:\n - 'one'",
            "packages:\n - 'one' # ignored\nother: x"
        )
        .is_empty());
        assert_eq!(crate::callgraph_store::rust_manifest_name_fields(b"[other]\nname = \"first\"\n[package]\nname = \"ignored\"\n[lib]\nname = \"one\"\nname = \"last\""), (Some("first".into()), Some("last".into())));
        assert_eq!(
            changed(
                "Cargo.toml",
                "[workspace]\nmembers = []",
                "[workspace]\nmembers = [\"new\"]"
            ),
            ["disk.workspace.members"]
        );
        assert_eq!(
            changed("Cargo.toml", "[lib]\npath = \"a\"", "[lib]\npath = \"b\""),
            ["disk.lib.path"]
        );
        // Invalid TOML still has line-oriented names for the manifest resolver.
        assert_eq!(
            changed(
                "Cargo.toml",
                "name = \"a\"\nname = \"x\"",
                "name = \"b\"\nname = \"x\""
            ),
            ["manifest.name"]
        );
        assert_eq!(
            changed(
                "Cargo.toml",
                "[lib]\nname = \"a\"\nname = \"x\"",
                "[lib]\nname = \"a\"\nname = \"y\""
            ),
            ["manifest.lib.name"]
        );
    }
}
