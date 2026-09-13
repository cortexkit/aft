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
