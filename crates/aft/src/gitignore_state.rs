use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Debug, PartialEq, Eq)]
struct IgnoreInput {
    path: PathBuf,
    scope: PathBuf,
    bytes: Vec<u8>,
    nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IgnoreInputSnapshot {
    root: PathBuf,
    inputs: Vec<IgnoreInput>,
    content_hash: blake3::Hash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum IgnoreRuleChange {
    Unchanged,
    AddOnly {
        retired_paths: Vec<PathBuf>,
        scopes: Vec<PathBuf>,
    },
    Scoped {
        affected_paths: Vec<PathBuf>,
        scopes: Vec<PathBuf>,
    },
    Full {
        scopes: Vec<PathBuf>,
    },
}

impl IgnoreInputSnapshot {
    pub(crate) fn collect(root: PathBuf, git_common_dir: Option<&Path>) -> Self {
        let mut inputs = Vec::new();
        if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
            push_input(&mut inputs, global_ignore, root.clone(), false);
        }

        let root_ignore = root.join(".gitignore");
        push_input(&mut inputs, root_ignore.clone(), root.clone(), false);
        let root_aftignore = root.join(".aftignore");
        push_input(&mut inputs, root_aftignore.clone(), root.clone(), false);

        let info_exclude = git_common_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join(".git"))
            .join("info")
            .join("exclude");
        push_input(&mut inputs, info_exclude, root.clone(), false);

        let walker = ignore::WalkBuilder::new(&root)
            .same_file_system(true)
            .standard_filters(true)
            .hidden(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    "node_modules" | "target" | ".git" | ".opencode" | ".alfonso"
                )
            })
            .build();
        let mut nested_ignore_files = walker
            .flatten()
            .filter_map(|entry| {
                let file_name = entry.file_name();
                let is_nested_gitignore = file_name == ".gitignore" && entry.path() != root_ignore;
                let is_nested_aftignore =
                    file_name == ".aftignore" && entry.path() != root_aftignore;
                (is_nested_gitignore || is_nested_aftignore).then(|| entry.into_path())
            })
            .collect::<Vec<_>>();
        nested_ignore_files.sort_by(|left, right| nested_input_order(&root, left, right));
        for path in nested_ignore_files {
            let scope = path.parent().unwrap_or(&root).to_path_buf();
            push_input(&mut inputs, path, scope, true);
        }

        let content_hash = hash_inputs(&root, &inputs);
        Self {
            root,
            inputs,
            content_hash,
        }
    }

    pub(crate) fn changed_paths_match_bytes(&self, changed_paths: &[PathBuf]) -> bool {
        !changed_paths.is_empty()
            && changed_paths.iter().all(|changed_path| {
                self.inputs
                    .iter()
                    .find(|input| same_path(&input.path, changed_path))
                    .is_some_and(|input| {
                        std::fs::read(changed_path).is_ok_and(|bytes| bytes == input.bytes)
                    })
            })
    }

    pub(crate) fn has_same_effective_inputs(&self, previous: Option<&Self>) -> bool {
        previous.is_some_and(|previous| {
            previous.content_hash == self.content_hash && previous.inputs == self.inputs
        })
    }

    pub(crate) fn classify_against(
        &self,
        old: Option<&Self>,
        old_matcher: Option<&Gitignore>,
        new_matcher: Option<&Gitignore>,
    ) -> IgnoreRuleChange {
        let Some(old) = old else {
            return IgnoreRuleChange::Full {
                scopes: vec![self.root.clone()],
            };
        };
        if self.has_same_effective_inputs(Some(old)) {
            return IgnoreRuleChange::Unchanged;
        }

        let scopes = changed_scopes(old, self);
        if additions_only(old, self) {
            let retired_paths = membership_changes(
                &scopes,
                old_matcher,
                new_matcher,
                MembershipChange::NewlyIgnored,
            );
            return IgnoreRuleChange::AddOnly {
                retired_paths,
                scopes,
            };
        }
        if scopes.iter().any(|scope| scope == &self.root) {
            IgnoreRuleChange::Full { scopes }
        } else {
            let affected_paths =
                membership_changes(&scopes, old_matcher, new_matcher, MembershipChange::Any);
            IgnoreRuleChange::Scoped {
                affected_paths,
                scopes,
            }
        }
    }

    pub(crate) fn build_matcher(&self) -> Option<Arc<Gitignore>> {
        let mut builder = GitignoreBuilder::new(&self.root);
        for input in &self.inputs {
            let contents = match std::str::from_utf8(&input.bytes) {
                Ok(contents) => contents,
                Err(error) => {
                    crate::slog_warn!(
                        "ignore rule input is not UTF-8 in {}: {}",
                        input.path.display(),
                        error
                    );
                    continue;
                }
            };
            for line in contents.lines() {
                let rewritten;
                let line = if input.nested {
                    let Some(relative_dir) = input
                        .path
                        .parent()
                        .and_then(|parent| parent.strip_prefix(&self.root).ok())
                    else {
                        continue;
                    };
                    let Some(value) =
                        crate::watcher_filter::rewrite_nested_ignore_line(relative_dir, line)
                    else {
                        continue;
                    };
                    rewritten = value;
                    rewritten.as_str()
                } else {
                    line
                };
                if let Err(error) = builder.add_line(Some(input.path.clone()), line) {
                    crate::slog_warn!(
                        "ignore rule parse error in {}: {}",
                        input.path.display(),
                        error
                    );
                }
            }
        }

        match builder.build() {
            Ok(matcher) if matcher.num_ignores() > 0 => Some(Arc::new(matcher)),
            Ok(_) => None,
            Err(error) => {
                crate::slog_warn!("gitignore matcher build failed: {}", error);
                None
            }
        }
    }
}

fn push_input(inputs: &mut Vec<IgnoreInput>, path: PathBuf, scope: PathBuf, nested: bool) {
    if !path.is_file() {
        return;
    }
    match std::fs::read(&path) {
        Ok(bytes) => inputs.push(IgnoreInput {
            path,
            scope,
            bytes,
            nested,
        }),
        Err(error) => crate::slog_warn!("ignore rule read error in {}: {}", path.display(), error),
    }
}

fn nested_input_order(root: &Path, left: &Path, right: &Path) -> std::cmp::Ordering {
    let left_relative = left.strip_prefix(root).unwrap_or(left);
    let right_relative = right.strip_prefix(root).unwrap_or(right);
    left_relative
        .components()
        .count()
        .cmp(&right_relative.components().count())
        .then_with(|| left_relative.parent().cmp(&right_relative.parent()))
        .then_with(|| {
            let left_is_aftignore = left.file_name().is_some_and(|name| name == ".aftignore");
            let right_is_aftignore = right.file_name().is_some_and(|name| name == ".aftignore");
            left_is_aftignore.cmp(&right_is_aftignore)
        })
}

fn hash_inputs(root: &Path, inputs: &[IgnoreInput]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"aft-effective-ignore-inputs-v1\0");
    hasher.update(root.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    for input in inputs {
        // The source path is part of the effective input: identical nested rule
        // bytes in two directories have different matcher semantics.
        let path = input.path.to_string_lossy();
        hasher.update(&(path.len() as u64).to_le_bytes());
        hasher.update(path.as_bytes());
        hasher.update(&(input.bytes.len() as u64).to_le_bytes());
        hasher.update(&input.bytes);
    }
    hasher.finalize()
}

fn same_path(left: &Path, right: &Path) -> bool {
    left == right
        || std::fs::canonicalize(left)
            .ok()
            .zip(std::fs::canonicalize(right).ok())
            .is_some_and(|(left, right)| left == right)
}

fn changed_scopes(old: &IgnoreInputSnapshot, new: &IgnoreInputSnapshot) -> Vec<PathBuf> {
    let old_inputs = old
        .inputs
        .iter()
        .map(|input| (input.path.clone(), input))
        .collect::<BTreeMap<_, _>>();
    let new_inputs = new
        .inputs
        .iter()
        .map(|input| (input.path.clone(), input))
        .collect::<BTreeMap<_, _>>();
    let mut scopes = BTreeSet::new();
    for path in old_inputs.keys().chain(new_inputs.keys()) {
        let changed = match (old_inputs.get(path), new_inputs.get(path)) {
            (Some(old), Some(new)) => old.bytes != new.bytes || old.scope != new.scope,
            _ => true,
        };
        if changed {
            let scope = old_inputs
                .get(path)
                .map(|input| &input.scope)
                .or_else(|| new_inputs.get(path).map(|input| &input.scope))
                .expect("changed ignore input has a scope");
            scopes.insert(scope.clone());
        }
    }
    collapse_scopes(scopes)
}

fn collapse_scopes(scopes: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut scopes = scopes.into_iter().collect::<Vec<_>>();
    scopes.sort_by_key(|scope| scope.components().count());
    let mut collapsed = Vec::<PathBuf>::new();
    for scope in scopes {
        if collapsed.iter().any(|parent| scope.starts_with(parent)) {
            continue;
        }
        collapsed.push(scope);
    }
    collapsed
}

fn additions_only(old: &IgnoreInputSnapshot, new: &IgnoreInputSnapshot) -> bool {
    let new_inputs = new
        .inputs
        .iter()
        .map(|input| (&input.path, input))
        .collect::<BTreeMap<_, _>>();
    let old_inputs = old
        .inputs
        .iter()
        .map(|input| (&input.path, input))
        .collect::<BTreeMap<_, _>>();
    let mut changed = false;
    for old_input in &old.inputs {
        let Some(new_input) = new_inputs.get(&old_input.path) else {
            return false;
        };
        if old_input.bytes == new_input.bytes {
            continue;
        }
        changed = true;
        if old_input.scope != new_input.scope
            || old_input.nested != new_input.nested
            || !appends_exclusions(&old_input.bytes, &new_input.bytes)
        {
            return false;
        }
    }
    for new_input in &new.inputs {
        if old_inputs.contains_key(&new_input.path) {
            continue;
        }
        changed = true;
        if !contains_only_exclusions(&new_input.bytes) {
            return false;
        }
    }
    changed
}

fn appends_exclusions(old: &[u8], new: &[u8]) -> bool {
    let Some(mut suffix) = new.strip_prefix(old) else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    if !old.is_empty() && !old.ends_with(b"\n") && !old.ends_with(b"\r") {
        let Some(stripped_suffix) = suffix
            .strip_prefix(b"\r\n")
            .or_else(|| suffix.strip_prefix(b"\n"))
            .or_else(|| suffix.strip_prefix(b"\r"))
        else {
            return false;
        };
        suffix = stripped_suffix;
        if suffix.is_empty() {
            return true;
        }
    }
    contains_only_exclusions(suffix)
}

fn contains_only_exclusions(bytes: &[u8]) -> bool {
    let Ok(contents) = std::str::from_utf8(bytes) else {
        return false;
    };
    contents
        .lines()
        .all(|line| line.is_empty() || line.starts_with('#') || !line.starts_with('!'))
}

#[derive(Clone, Copy)]
enum MembershipChange {
    Any,
    NewlyIgnored,
}

fn membership_changes(
    scopes: &[PathBuf],
    old_matcher: Option<&Gitignore>,
    new_matcher: Option<&Gitignore>,
    change: MembershipChange,
) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for scope in scopes {
        let mut builder = ignore::WalkBuilder::new(scope);
        builder
            .same_file_system(true)
            .standard_filters(false)
            .hidden(false)
            .filter_entry(|entry| {
                let name = entry.file_name().to_string_lossy();
                if entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    return !matches!(
                        name.as_ref(),
                        "node_modules"
                            | "target"
                            | "venv"
                            | ".venv"
                            | ".git"
                            | "__pycache__"
                            | ".tox"
                            | "dist"
                            | "build"
                    );
                }
                true
            });
        for entry in builder.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.into_path();
            let old_ignored = matcher_ignores(old_matcher, &path);
            let new_ignored = matcher_ignores(new_matcher, &path);
            let include = match change {
                MembershipChange::Any => old_ignored != new_ignored,
                MembershipChange::NewlyIgnored => !old_ignored && new_ignored,
            };
            if include {
                paths.insert(path);
            }
        }
    }
    paths.into_iter().collect()
}

fn matcher_ignores(matcher: Option<&Gitignore>, path: &Path) -> bool {
    matcher.is_some_and(|matcher| {
        path.starts_with(matcher.path())
            && matcher
                .matched_path_or_any_parents(path, path.is_dir())
                .is_ignore()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(root: &Path, bytes: &[u8]) -> IgnoreInputSnapshot {
        let path = root.join("nested/.gitignore");
        let input = IgnoreInput {
            scope: path.parent().unwrap().to_path_buf(),
            path,
            bytes: bytes.to_vec(),
            nested: true,
        };
        IgnoreInputSnapshot {
            root: root.to_path_buf(),
            content_hash: hash_inputs(root, std::slice::from_ref(&input)),
            inputs: vec![input],
        }
    }

    #[test]
    fn add_only_requires_byte_prefix_and_new_positive_rules() {
        let root = Path::new("/workspace");
        let old = snapshot(root, b"cache/\n");
        let appended = snapshot(root, b"cache/\ngenerated/\n");
        let removal = snapshot(root, b"");
        let negation = snapshot(root, b"cache/\n!cache/keep.rs\n");
        assert!(additions_only(&old, &appended));
        assert!(!additions_only(&old, &removal));
        assert!(!additions_only(&old, &negation));
    }

    #[test]
    fn extending_newline_less_exclusion_is_not_add_only() {
        let root = Path::new("/workspace");
        let old = snapshot(root, b"build");
        let extended_rule = snapshot(root, b"build*.log");

        assert!(!additions_only(&old, &extended_rule));
    }

    #[test]
    fn extending_newline_less_exclusion_with_negation_is_not_add_only() {
        let root = Path::new("/workspace");
        let old = snapshot(root, b"cache");
        let extended_rule = snapshot(root, b"cache!cache/keep.rs");

        assert!(!additions_only(&old, &extended_rule));
    }

    #[test]
    fn newline_before_appended_exclusion_remains_add_only() {
        let root = Path::new("/workspace");
        let old = snapshot(root, b"build");
        let appended_rule = snapshot(root, b"build\n*.log");

        assert!(additions_only(&old, &appended_rule));
    }
}
