use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use super::{cargo_workspace_excludes, observe_path};
use crate::lsp::applicability_cache::capture_id;
use crate::walk_boundary::DeviceBoundary;

type MembersByRoot = HashMap<PathBuf, HashSet<PathBuf>>;

thread_local! {
    static WALK_MEMBERS: RefCell<(Option<u64>, MembersByRoot)> = RefCell::default();
}

pub(super) fn is_implicit_member(
    root: &Path,
    candidate: &Path,
    manifest: &toml::Value,
    workspace: &toml::Table,
    patterns: &[toml::Value],
) -> bool {
    let Some(id) = capture_id() else {
        return collect_members(root, manifest, workspace, patterns).contains(candidate);
    };
    // Reuse the manifest graph across files in one applicability walk. The
    // next walk must read it again so it observes edits to member manifests.
    WALK_MEMBERS.with_borrow_mut(|(cached_id, roots)| {
        if *cached_id != Some(id) {
            roots.clear();
            *cached_id = Some(id);
        }
        roots
            .entry(root.to_path_buf())
            .or_insert_with(|| collect_members(root, manifest, workspace, patterns))
            .contains(candidate)
    })
}

fn collect_members(
    root: &Path,
    manifest: &toml::Value,
    workspace: &toml::Table,
    patterns: &[toml::Value],
) -> HashSet<PathBuf> {
    let Ok(device) = DeviceBoundary::for_root(root) else {
        return HashSet::new();
    };
    let mut pending = Vec::new();
    if manifest.get("package").is_some() {
        pending.push(root.to_path_buf());
    }
    for pattern in patterns.iter().filter_map(toml::Value::as_str) {
        expand_member(root, pattern.trim(), workspace, &device, &mut pending);
    }
    let explicit: HashSet<_> = pending
        .iter_mut()
        .map(|dir| {
            *dir = crate::path_identity::canonicalize_normalized(dir);
            dir.clone()
        })
        .collect();
    let mut seen = HashSet::new();
    let mut members = HashSet::new();
    while let Some(dir) = pending.pop() {
        let Ok(relative) = dir.strip_prefix(root) else {
            continue;
        };
        if !seen.insert(dir.clone())
            || (!explicit.contains(&dir) && cargo_workspace_excludes(workspace, relative))
            || !device.should_descend(&dir).unwrap_or(false)
        {
            continue;
        }
        let manifest_path = dir.join("Cargo.toml");
        observe_path(&manifest_path);
        let Ok(contents) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        let Ok(member) = contents.parse::<toml::Value>() else {
            continue;
        };
        if member.get("package").is_none() || (dir != root && member.get("workspace").is_some()) {
            continue;
        }
        dependency_paths(&member, |name, dependency| {
            let direct = dependency.get("path").and_then(toml::Value::as_str);
            let path = if let Some(path) = direct {
                dir.join(path)
            } else if dependency.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                let Some(path) = workspace
                    .get("dependencies")
                    .and_then(|deps| deps.get(name))
                    .and_then(|dep| dep.get("path"))
                    .and_then(toml::Value::as_str)
                else {
                    return;
                };
                root.join(path)
            } else {
                return;
            };
            let path = crate::path_identity::normalize_path(&path);
            if path.starts_with(root) {
                observe_path(&path);
                pending.push(crate::path_identity::canonicalize_normalized(&path));
            }
        });
        members.insert(dir);
    }
    members
}

fn dependency_paths(manifest: &toml::Value, mut visit: impl FnMut(&str, &toml::Value)) {
    let targets = manifest
        .get("target")
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(toml::Table::values);
    for section in std::iter::once(manifest).chain(targets) {
        for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(dependencies) = section.get(kind).and_then(toml::Value::as_table) {
                for (name, dependency) in dependencies {
                    visit(name, dependency);
                }
            }
        }
    }
}

fn expand_member(
    root: &Path,
    pattern: &str,
    workspace: &toml::Table,
    device: &DeviceBoundary,
    out: &mut Vec<PathBuf>,
) {
    let mut prefix = root.to_path_buf();
    for component in Path::new(pattern).components() {
        match component {
            Component::CurDir => (),
            Component::Normal(name) if !name.to_string_lossy().contains(['*', '?', '[', '{']) => {
                prefix.push(name);
            }
            Component::Normal(_) => break,
            _ => return,
        }
    }
    if prefix == root.join(pattern) {
        out.push(prefix);
        return;
    }
    let Ok(glob) = globset::GlobBuilder::new(pattern.trim_start_matches("./"))
        .literal_separator(true)
        .build()
    else {
        return;
    };
    let glob = glob.compile_matcher();
    let max_depth = if pattern.contains("**") {
        usize::MAX
    } else {
        Path::new(pattern).components().count()
    };
    let prefix = crate::path_identity::canonicalize_normalized(&prefix);
    let mut dirs = vec![prefix];
    while let Some(dir) = dirs.pop() {
        let Ok(relative) = dir.strip_prefix(root) else {
            continue;
        };
        if !device.should_descend(&dir).unwrap_or(false) {
            continue;
        }
        if glob.is_match(relative) && !cargo_workspace_excludes(workspace, relative) {
            let manifest = dir.join("Cargo.toml");
            observe_path(&manifest);
            if manifest.is_file() {
                out.push(dir.clone());
            }
        }
        if relative.components().count() < max_depth {
            observe_path(&dir);
            if let Ok(entries) = std::fs::read_dir(&dir) {
                dirs.extend(
                    entries
                        .flatten()
                        .filter_map(|entry| entry.file_type().ok()?.is_dir().then(|| entry.path())),
                );
            }
        }
    }
}
