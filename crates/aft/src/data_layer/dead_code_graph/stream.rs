//! Bounded traversal of the rollup's file projection for the offline gates.

use std::collections::BTreeMap;

use super::{parent, DeadCodeGraph, FileProjection, SeedClass};
use crate::data_layer::dead_code_snapshot::CallgraphExport;

impl DeadCodeGraph<'_> {
    #[cfg(test)]
    pub(crate) fn held_full_files(&self) -> usize {
        self.files
            .values()
            .filter(|cell| cell.full.get().is_some())
            .count()
    }

    pub(crate) fn prepare_stream(&self) -> Result<(), String> {
        self.compact_targets(true);
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
