use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Debug, PartialEq, Eq)]
struct IgnoreInput {
    path: PathBuf,
    bytes: Vec<u8>,
    nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IgnoreInputSnapshot {
    root: PathBuf,
    inputs: Vec<IgnoreInput>,
    content_hash: blake3::Hash,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IgnoreRuleChange {
    Unchanged,
    Changed,
}

impl IgnoreInputSnapshot {
    pub(crate) fn collect(root: PathBuf, git_common_dir: Option<&Path>) -> Self {
        let mut inputs = Vec::new();
        if let Some(global_ignore) = ignore::gitignore::gitconfig_excludes_path() {
            push_input(&mut inputs, global_ignore, false);
        }

        let root_ignore = root.join(".gitignore");
        push_input(&mut inputs, root_ignore.clone(), false);
        let root_aftignore = root.join(".aftignore");
        push_input(&mut inputs, root_aftignore.clone(), false);

        let info_exclude = git_common_dir
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.join(".git"))
            .join("info")
            .join("exclude");
        push_input(&mut inputs, info_exclude, false);

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
            push_input(&mut inputs, path, true);
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

fn push_input(inputs: &mut Vec<IgnoreInput>, path: PathBuf, nested: bool) {
    if !path.is_file() {
        return;
    }
    match std::fs::read(&path) {
        Ok(bytes) => inputs.push(IgnoreInput {
            path,
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
