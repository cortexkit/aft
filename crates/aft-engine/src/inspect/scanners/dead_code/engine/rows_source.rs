//! Adapt the gate's independent file rows to the legacy finding renderer.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{
    aggregate, CallSource, CallgraphSnapshot, FileContribution, FileInputs, ProjectRoles, SeedClass,
};
use crate::callgraph_store::dead_code_projection::UNKNOWN_RECEIVER_LIVE_PROVENANCE;
use crate::data_layer::dead_code::stream::{File, Rows};

struct RowsSource<'r, 'v> {
    rows: &'r Rows<'v>,
    calls: BTreeMap<String, Vec<&'r str>>,
    facts: BTreeMap<String, Vec<&'r str>>,
}

impl<'r, 'v> RowsSource<'r, 'v> {
    fn new(rows: &'r Rows<'v>) -> Self {
        let root = rows.root();
        let lexical = super::normalize_path(root);
        let canonical = super::super::canonicalize_normalized(root);
        let mut calls = BTreeMap::<_, Vec<_>>::new();
        let mut facts = BTreeMap::<_, Vec<_>>::new();
        for (file, path) in rows.paths() {
            let comparison = super::caller_comparison_path(root, &lexical, &canonical, file);
            let key = super::caller_comparison_key(&lexical, &canonical, comparison).0;
            calls.entry(key).or_default().push(path.as_str());
            facts
                .entry(super::relative_path(root, file))
                .or_default()
                .push(path.as_str());
        }
        for paths in facts.values_mut() {
            paths.sort_unstable();
        }
        Self { rows, calls, facts }
    }

    fn split_calls(
        &self,
        file: File,
        inputs: &mut FileInputs<'_>,
        calls: bool,
        classify: &dyn Fn(u32) -> SeedClass,
    ) {
        for call in file.calls {
            if call.provenance == UNKNOWN_RECEIVER_LIVE_PROVENANCE {
                let target = super::super::parse_target(self.root(), &call.target);
                if let (Some(file), Some(symbol)) = (target.file, target.symbol) {
                    inputs
                        .unknown_live
                        .push((file, symbol, classify(call.line)));
                }
            } else if calls {
                inputs.owned.push(call);
            }
        }
    }
}

impl CallSource for RowsSource<'_, '_> {
    fn root(&self) -> &Path {
        self.rows.root()
    }

    fn file_inputs(
        &self,
        file: &str,
        calls: bool,
        classify: &dyn Fn(u32) -> SeedClass,
    ) -> Result<FileInputs<'_>, String> {
        let mut inputs = FileInputs::default();
        for path in self.calls.get(file).into_iter().flatten() {
            self.split_calls(self.rows.file(path)?, &mut inputs, calls, classify);
        }
        inputs.unknown_live.sort_unstable();
        inputs.unknown_live.dedup();
        if !inputs.unknown_live.is_empty() {
            inputs.consumed.push(file.to_owned());
        }
        if calls {
            for path in self.facts.get(file).into_iter().flatten() {
                let facts = self.rows.file(path)?;
                if !facts.roots.is_empty() {
                    inputs
                        .snapshot_roots
                        .get_or_insert_with(BTreeSet::new)
                        .extend(facts.roots);
                }
                if let Some(go) = &facts.go {
                    inputs.go_initializers =
                        Some(super::relative_initializer_group(self.root(), go));
                }
            }
        }
        Ok(inputs)
    }

    fn other_unknown_live(
        &self,
        consumed: &BTreeSet<String>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut roots = BTreeSet::new();
        for (file, paths) in &self.calls {
            if consumed.contains(file) {
                continue;
            }
            for path in paths {
                let mut inputs = FileInputs::default();
                self.split_calls(self.rows.file(path)?, &mut inputs, false, &|_| {
                    SeedClass::Production
                });
                roots.extend(
                    inputs
                        .unknown_live
                        .into_iter()
                        .map(|(file, symbol, _)| (file, symbol)),
                );
            }
        }
        Ok(roots.into_iter().collect())
    }
}

pub(crate) fn aggregate_with_rows(
    rows: &Rows<'_>,
    snapshot: &CallgraphSnapshot,
    contributions: &[FileContribution],
    public_api_files: &BTreeSet<String>,
    roles: &ProjectRoles,
    timings: &mut Vec<(&'static str, f64)>,
) -> Result<serde_json::Value, String> {
    super::with_job_canonical_paths(|| {
        let source = RowsSource::new(rows);
        aggregate(
            &source,
            snapshot,
            contributions,
            public_api_files,
            roles,
            timings,
        )
    })
}
