//! Bounded traversal of the rollup's file projection for the offline gates.

use std::borrow::Cow;
use std::collections::BTreeMap;

use super::{parent, DeadCodeGraph, FileProjection, IndexedFile, SeedClass, Target, TargetIndex};
use crate::data_layer::dead_code::stream::cache::TargetCache;
use crate::data_layer::dead_code_snapshot::CallgraphExport;

pub(super) type Targets = TargetCache<TargetIndex>;

impl DeadCodeGraph<'_> {
    #[cfg(test)]
    pub(crate) fn held_full_files(&self) -> usize {
        self.files
            .values()
            .filter(|cell| cell.full.get().is_some())
            .count()
    }

    pub(crate) fn prepare_stream(&self) -> Result<(), String> {
        self.stream_targets.get_or_init(parking_lot::Mutex::default);
        let mut packages = BTreeMap::<_, Vec<_>>::new();
        for path in self.file_paths() {
            if self.source.parse(path)?.language == "go" {
                packages
                    .entry(parent(path))
                    .or_default()
                    .push(path.to_owned());
            }
        }
        self.seed_go_packages(packages);
        Ok(())
    }

    pub(crate) fn held_stream_targets(&self) -> (usize, usize) {
        self.stream_targets
            .get()
            .map_or((0, 0), |cache| cache.lock().held())
    }

    fn stream_target<R>(
        &self,
        cache: &parking_lot::Mutex<Targets>,
        path: &str,
        read: impl FnOnce(Target<'_>) -> R,
    ) -> Option<R> {
        let cell = self.files.get(path)?;
        if let Some(Some(full)) = cell.full.get() {
            return Some(read(Target::Full(full)));
        }
        cache.lock().read(
            path,
            || {
                let parse = match self.source.parse(path) {
                    Ok(parse) => parse,
                    Err(error) => {
                        self.failure.lock().get_or_insert(error);
                        return None;
                    }
                };
                let index = TargetIndex::of(&IndexedFile::build(path, &parse));
                let bytes = index.text.len()
                    + std::mem::size_of_val(&*index.keys)
                    + std::mem::size_of_val(&*index.ids);
                Some((bytes, index))
            },
            |index| read(Target::Index(index)),
        )
    }

    pub(super) fn target_lookup(&self, path: &str, symbol: &str) -> Option<String> {
        if let Some(cache) = self.stream_targets.get() {
            return self
                .stream_target(cache, path, |target| {
                    target.lookup(symbol).map(str::to_owned)
                })
                .flatten();
        }
        self.target(path)?.lookup(symbol).map(str::to_owned)
    }

    pub(super) fn target_scoped(&self, path: &str, symbol: &str) -> Option<String> {
        if let Some(cache) = self.stream_targets.get() {
            return self
                .stream_target(cache, path, |target| {
                    target.scoped(symbol).map(str::to_owned)
                })
                .flatten();
        }
        self.target(path)?.scoped(symbol).map(str::to_owned)
    }

    pub(super) fn target_name(&self, path: &str, id: &str) -> Option<Cow<'_, str>> {
        if let Some(cache) = self.stream_targets.get() {
            return self
                .stream_target(cache, path, |target| {
                    target.node(id).map(|(name, _)| Cow::Owned(name.to_owned()))
                })
                .flatten();
        }
        self.target(path)?
            .node(id)
            .map(|(name, _)| Cow::Borrowed(name))
    }

    pub(super) fn target_is_constructor(&self, path: &str, id: &str) -> bool {
        let is_constructor = |target: Target<'_>| {
            target
                .node(id)
                .is_some_and(|(_, kind)| matches!(kind, "function" | "method"))
        };
        if let Some(cache) = self.stream_targets.get() {
            return self
                .stream_target(cache, path, is_constructor)
                .unwrap_or(false);
        }
        self.target(path).is_some_and(is_constructor)
    }

    pub(crate) fn stream_file(
        &mut self,
        path: &str,
    ) -> Result<
        (
            FileProjection,
            Vec<CallgraphExport>,
            std::collections::BTreeSet<String>,
        ),
        String,
    > {
        let projection = self.project_file_in(path, &SeedClass::TestLine, true, true)?;
        let exports = self.exported_symbols(path);
        let roots = self.own_entry_point_symbols(path);
        // Calls from later files still need the compact target index, but
        // not this caller's full symbol table.
        self.files
            .get_mut(path)
            .expect("streamed file exists")
            .full
            .take();
        if let Some(error) = self.failure() {
            return Err(error);
        }
        Ok((projection, exports, roots))
    }
}
