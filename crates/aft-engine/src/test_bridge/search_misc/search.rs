use std::path::Path;

use crate::pattern_compile::CompiledPattern;
use crate::search_index::{
    GrepPathExclusion, GrepQueryPhaseTimings, GrepResult, GrepScanOrder, PathFilters, RegexQuery,
    SearchIndexSnapshot,
};
use crate::search_state::request_files::{FileStat, RequestFile};

pub fn unshared_request_file(bytes: Vec<u8>, stat: Option<FileStat>) -> RequestFile {
    RequestFile::unshared(bytes, stat)
}

pub trait SearchSnapshotTest {
    fn file_entry_count(&self) -> usize;
    fn has_file_in_scope(&self, search_root: &Path) -> bool;
    fn search_grep_profiled_with_filters(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)>;
    #[allow(clippy::too_many_arguments)]
    fn search_grep_profiled_with_filters_and_query_in_order(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)>;
}

impl SearchSnapshotTest for SearchIndexSnapshot {
    fn file_entry_count(&self) -> usize {
        self.file_entries().len()
    }
    fn has_file_in_scope(&self, search_root: &Path) -> bool {
        self.has_file_in_scope(search_root)
    }
    fn search_grep_profiled_with_filters(
        &self,
        pattern: &CompiledPattern,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters(
            pattern,
            filters,
            search_root,
            max_results,
            path_exclusion,
        )
    }
    fn search_grep_profiled_with_filters_and_query_in_order(
        &self,
        pattern: &CompiledPattern,
        query: &RegexQuery,
        filters: &PathFilters,
        search_root: &Path,
        max_results: usize,
        path_exclusion: Option<GrepPathExclusion>,
        order: GrepScanOrder,
    ) -> std::io::Result<(GrepResult, GrepQueryPhaseTimings)> {
        self.search_grep_profiled_with_filters_and_query_in_order(
            pattern,
            query,
            filters,
            search_root,
            max_results,
            path_exclusion,
            order,
        )
    }
}
