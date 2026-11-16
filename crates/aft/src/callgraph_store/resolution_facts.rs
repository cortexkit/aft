pub use aft_parse::callgraph_store::resolution_facts::*;

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
        assert_eq!(
            changed(
                "Cargo.toml",
                "[[bin]]\nname = \"x\"\npath = \"a.rs\"",
                "[[bin]]\nname = \"x\"\npath = \"b.rs\""
            ),
            ["disk.targets"]
        );
        assert_eq!(
            changed(
                "Cargo.toml",
                "[package]\nname = \"p\"",
                "[package]\nname = \"p\"\nautotests = false"
            ),
            ["disk.targets"]
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
