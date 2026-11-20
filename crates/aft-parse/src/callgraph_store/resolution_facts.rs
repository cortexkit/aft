//! Resolver input projections. Digests describe consumed fields, never file bytes.
use std::collections::BTreeMap;

pub type FactDigests = BTreeMap<String, String>;

pub fn project(path: &[u8], bytes: &[u8]) -> Option<FactDigests> {
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
            // What the crate-target reader consumes besides `[lib] path`.
            let section = |name: &str| value.as_ref().and_then(|v| v.get(name));
            let auto = |key: &str| section("package").and_then(|package| package.get(key));
            insert(
                "disk.targets",
                serde_json::to_value((
                    section("bin"),
                    section("example"),
                    section("test"),
                    section("bench"),
                    auto("autobins"),
                    auto("autoexamples"),
                    auto("autotests"),
                    auto("autobenches"),
                ))
                .expect("TOML value"),
            );
        }
        _ => return None,
    }
    Some(facts)
}
