//! A checkout's call graph between publications: the stored slices of its
//! last publication, with the files resolved since laid over them.
//!
//! Every row of the call graph belongs to the file whose resolution wrote
//! it, so an overlay works file by file: a file the overlay holds answers
//! from the overlay, a file it removed answers nothing, and every other file
//! answers from the base. A lookup takes the base's matching files that the
//! overlay does not replace, plus the overlay's own matches.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::query::{MemorySlices, SliceSource};
use super::slice::FileSlice;

/// Slices from `base`, with replaced and removed files laid over them.
#[derive(Clone)]
pub(crate) struct OverlaySlices<B> {
    base: B,
    /// The files resolved since `base` was published.
    replaced: MemorySlices,
    /// The files removed since `base` was published.
    removed: BTreeSet<String>,
}

impl<B: SliceSource> OverlaySlices<B> {
    pub(crate) fn new(base: B) -> Self {
        Self {
            base,
            replaced: MemorySlices::default(),
            removed: BTreeSet::new(),
        }
    }

    /// Lays a file's new slice over the base.
    pub(crate) fn replace(&mut self, slice: FileSlice) {
        self.removed.remove(slice.path());
        self.replaced.insert(slice);
    }

    /// Lays `path`'s new slice over the base, assembled from `parse` and
    /// `resolution` when a query first reads it (see
    /// [`MemorySlices::insert_lazy`]).
    pub(crate) fn replace_lazy(
        &mut self,
        path: &str,
        record_key: &str,
        parse: Arc<crate::callgraph_store::join::ParseBlob>,
        resolution: Arc<super::callgraph::FileResolution>,
    ) {
        self.removed.remove(path);
        self.replaced
            .insert_lazy(path, record_key, parse, resolution);
    }

    /// Removes a file from the checkout.
    pub(crate) fn remove(&mut self, path: &str) {
        self.replaced.remove(path);
        self.removed.insert(path.to_string());
    }

    /// How many files the overlay replaces or removes.
    pub(crate) fn len(&self) -> usize {
        self.replaced.files().len() + self.removed.len()
    }

    /// The slices the overlay lies over.
    pub(crate) fn base(&self) -> &B {
        &self.base
    }

    /// Whether the base still answers for `path`.
    fn answers_from_base(&self, path: &str) -> bool {
        !self.removed.contains(path) && self.replaced.slice(path).is_none()
    }

    fn merge(&self, base: BTreeSet<String>, own: BTreeSet<String>) -> BTreeSet<String> {
        let mut files = base
            .into_iter()
            .filter(|path| self.answers_from_base(path))
            .collect::<BTreeSet<_>>();
        files.extend(own);
        files
    }
}

impl<B: SliceSource> SliceSource for OverlaySlices<B> {
    fn slice(&self, path: &str) -> Option<Arc<FileSlice>> {
        if self.removed.contains(path) {
            return None;
        }
        self.replaced.slice(path).or_else(|| self.base.slice(path))
    }

    fn caller_files(&self, target_file: &str, target_symbol: &str) -> BTreeSet<String> {
        self.merge(
            self.base.caller_files(target_file, target_symbol),
            self.replaced.caller_files(target_file, target_symbol),
        )
    }

    fn files_with_ref_name(&self, short_name: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_with_ref_name(short_name),
            self.replaced.files_with_ref_name(short_name),
        )
    }

    fn files_with_symbol_name(&self, name: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_with_symbol_name(name),
            self.replaced.files_with_symbol_name(name),
        )
    }

    fn files_of_language(&self, language: &str) -> BTreeSet<String> {
        self.merge(
            self.base.files_of_language(language),
            self.replaced.files_of_language(language),
        )
    }

    fn files(&self) -> Vec<String> {
        let mut files = self
            .base
            .files()
            .into_iter()
            .filter(|path| !self.removed.contains(path))
            .collect::<BTreeSet<_>>();
        files.extend(self.replaced.files());
        files.into_iter().collect()
    }
}

impl<B: aft_store::heap::HeapBytes> aft_store::heap::HeapBytes for OverlaySlices<B> {
    fn heap_bytes(&self, seen: &mut aft_store::heap::Seen) -> usize {
        let Self {
            base,
            replaced,
            removed,
        } = self;
        base.heap_bytes(seen) + replaced.heap_bytes(seen) + removed.heap_bytes(seen)
    }
}
