//! The parts of workspace manifests and tsconfig files that module resolution
//! reads, and which importers a change to them can move.
//!
//! The field list follows `crate::callgraph`'s resolver, not the manifest
//! formats:
//! - `package.json`: `name` (package lookup by name, both for ancestors and
//!   for workspace members), `exports`, `module` and `main` (the package entry
//!   and subpaths), and `workspaces` (which directories are members and which
//!   directory is a workspace root). Whether the file exists also matters: a
//!   directory is a member candidate only when it has one.
//! - `pnpm-workspace.yaml`: the `packages` patterns, parsed the way the
//!   resolver parses them.
//! - `tsconfig.json`: whether it exists (the nearest one wins), and
//!   `compilerOptions.paths` and `compilerOptions.baseUrl`.
//!
//! Everything else (`version`, `dependencies`, `scripts`, ...) is ignored, so a
//! release that bumps every version changes nothing here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolutionConfigKind {
    PackageJson,
    PnpmWorkspace,
    Tsconfig,
}

/// Fields of `package.json` that decide what a package name resolves to.
const PACKAGE_ENTRY_FIELDS: [&str; 4] = ["name", "exports", "module", "main"];

pub(crate) fn resolution_config_kind(path: &Path) -> Option<ResolutionConfigKind> {
    resolution_config_kind_for_name(path.file_name()?.to_str()?)
}

fn resolution_config_kind_for_name(name: &str) -> Option<ResolutionConfigKind> {
    match name {
        "package.json" => Some(ResolutionConfigKind::PackageJson),
        "pnpm-workspace.yaml" => Some(ResolutionConfigKind::PnpmWorkspace),
        "tsconfig.json" => Some(ResolutionConfigKind::Tsconfig),
        _ => None,
    }
}

/// The resolution fields of `path` as canonical JSON text, or None when the
/// file does not exist. An unreadable or unparsable file is present with no
/// fields, which is how the resolver treats it too.
pub(crate) fn resolution_fields(path: &Path) -> Option<String> {
    let kind = resolution_config_kind(path)?;
    match std::fs::read(path) {
        Ok(bytes) => Some(resolution_fields_from_bytes(kind, &bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(Value::Object(Map::new()).to_string()),
    }
}

pub(crate) fn resolution_fields_from_bytes(kind: ResolutionConfigKind, bytes: &[u8]) -> String {
    let mut fields = Map::new();
    match kind {
        ResolutionConfigKind::PackageJson => {
            if let Ok(Value::Object(package)) = serde_json::from_slice::<Value>(bytes) {
                for field in PACKAGE_ENTRY_FIELDS.iter().chain(["workspaces"].iter()) {
                    if let Some(value) = package.get(*field) {
                        fields.insert((*field).to_string(), value.clone());
                    }
                }
            }
        }
        ResolutionConfigKind::PnpmWorkspace => {
            let patterns = crate::callgraph::parse_pnpm_workspace_patterns(bytes);
            fields.insert(
                "packages".to_string(),
                Value::Array(patterns.into_iter().map(Value::String).collect()),
            );
        }
        ResolutionConfigKind::Tsconfig => {
            if let Ok(tsconfig) = serde_json::from_slice::<Value>(bytes) {
                if let Some(options) = tsconfig.get("compilerOptions") {
                    for field in ["paths", "baseUrl"] {
                        if let Some(value) = options.get(field) {
                            fields.insert(field.to_string(), value.clone());
                        }
                    }
                }
            }
        }
    }
    Value::Object(fields).to_string()
}

/// Which stored references a manifest change can move.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ResolutionChange {
    /// Directory holding the changed file, relative to the project root
    /// (empty for the root itself).
    pub(crate) dir: String,
    /// Importers of these package names (and their subpaths) resolve
    /// differently: the old and new `name` of a package whose entry fields
    /// changed.
    pub(crate) package_names: BTreeSet<String>,
    /// Every non-relative import under `dir` can resolve differently:
    /// workspace membership or tsconfig path aliases changed.
    pub(crate) bare_imports_under_dir: bool,
}

/// How the change from `old` to `new` resolution fields (None: file absent) of
/// the config file at `rel_path` can move resolution. None when nothing the
/// resolver reads changed.
pub(crate) fn resolution_change(
    rel_path: &str,
    old: Option<&str>,
    new: Option<&str>,
) -> Option<ResolutionChange> {
    if old == new {
        return None;
    }
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let kind = resolution_config_kind_for_name(name)?;
    let dir = rel_path
        .rsplit_once('/')
        .map(|(dir, _)| dir.to_string())
        .unwrap_or_default();
    let parse = |fields: Option<&str>| -> Map<String, Value> {
        fields
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default()
    };
    let (old_fields, new_fields) = (parse(old), parse(new));
    let presence_changed = old.is_some() != new.is_some();
    let mut change = ResolutionChange {
        dir,
        ..ResolutionChange::default()
    };
    match kind {
        ResolutionConfigKind::PackageJson => {
            let entry_changed = presence_changed
                || PACKAGE_ENTRY_FIELDS
                    .iter()
                    .any(|field| old_fields.get(*field) != new_fields.get(*field));
            if entry_changed {
                for fields in [&old_fields, &new_fields] {
                    if let Some(Value::String(name)) = fields.get("name") {
                        change.package_names.insert(name.clone());
                    }
                }
            }
            change.bare_imports_under_dir =
                old_fields.get("workspaces") != new_fields.get("workspaces");
        }
        ResolutionConfigKind::PnpmWorkspace | ResolutionConfigKind::Tsconfig => {
            change.bare_imports_under_dir = true;
        }
    }
    Some(change)
}

/// The manifests configure watches for resolution changes: the root's
/// `package.json`, `pnpm-workspace.yaml` and `tsconfig.json`, and each
/// `packages/*` directory's `package.json` and `tsconfig.json`. Missing files
/// are included; their absence is part of the fingerprint.
pub(crate) fn workspace_resolution_manifest_paths(project_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![
        project_root.join("package.json"),
        project_root.join("pnpm-workspace.yaml"),
        project_root.join("tsconfig.json"),
    ];
    if let Ok(entries) = std::fs::read_dir(project_root.join("packages")) {
        let mut packages = entries
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        packages.sort();
        for package in packages {
            paths.push(package.join("package.json"));
            paths.push(package.join("tsconfig.json"));
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(text: &str) -> String {
        resolution_fields_from_bytes(ResolutionConfigKind::PackageJson, text.as_bytes())
    }

    #[test]
    fn version_and_dependency_changes_leave_package_fields_unchanged() {
        let before = package(r#"{"name":"@s/core","version":"1.0.0","main":"index.ts"}"#);
        let after = package(
            r#"{"name":"@s/core","version":"1.0.1","main":"index.ts","dependencies":{"left-pad":"^1"},"scripts":{"build":"tsc"}}"#,
        );
        assert_eq!(before, after);
        assert_eq!(
            resolution_change("packages/core/package.json", Some(&before), Some(&after)),
            None
        );
    }

    #[test]
    fn exports_change_names_the_package_importers() {
        let before = package(r#"{"name":"@s/core","exports":{".":"./a.ts"}}"#);
        let after = package(r#"{"name":"@s/core","exports":{".":"./b.ts"}}"#);
        let change = resolution_change("packages/core/package.json", Some(&before), Some(&after))
            .expect("exports is read by the resolver");
        assert_eq!(change.dir, "packages/core");
        assert_eq!(
            change.package_names,
            BTreeSet::from(["@s/core".to_string()])
        );
        assert!(!change.bare_imports_under_dir);
    }

    #[test]
    fn rename_names_both_packages_and_workspaces_change_reaches_bare_imports() {
        let before = package(r#"{"name":"old","workspaces":["packages/*"]}"#);
        let after = package(r#"{"name":"new","workspaces":["packages/*","tools/*"]}"#);
        let change = resolution_change("package.json", Some(&before), Some(&after)).unwrap();
        assert_eq!(change.dir, "");
        assert_eq!(
            change.package_names,
            BTreeSet::from(["new".to_string(), "old".to_string()])
        );
        assert!(change.bare_imports_under_dir);
    }

    #[test]
    fn tsconfig_fields_are_paths_and_base_url() {
        let tsconfig = |text: &str| {
            resolution_fields_from_bytes(ResolutionConfigKind::Tsconfig, text.as_bytes())
        };
        assert_eq!(
            tsconfig(r#"{"compilerOptions":{"strict":true,"paths":{"@/*":["src/*"]}}}"#),
            tsconfig(r#"{"compilerOptions":{"strict":false,"paths":{"@/*":["src/*"]}}}"#)
        );
        assert_ne!(
            tsconfig(r#"{"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#),
            tsconfig(r#"{"compilerOptions":{"paths":{"@/*":["lib/*"]}}}"#)
        );
        let created = resolution_change("app/tsconfig.json", None, Some(&tsconfig("{}")))
            .expect("a new tsconfig shadows its parents");
        assert!(created.bare_imports_under_dir);
    }
}
