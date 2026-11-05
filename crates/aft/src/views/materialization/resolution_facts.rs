//! Changed resolver inputs between two view manifests, compared through the
//! field projections in `callgraph_store::resolution_facts`.

use crate::callgraph_store::resolution_facts::project;

#[derive(Default)]
pub(super) struct InputDiff {
    pub changed: std::collections::BTreeSet<(String, String)>,
    pub inputs_changed: bool,
    pub unknown: usize,
}

/// Project each changed input once per side of the transition. Resolver reads
/// persist identities, not config bytes or recomputed per-reference digests.
pub(super) fn diff_inputs(
    base: &crate::views::Manifest,
    next: &crate::views::Manifest,
    changed: &std::collections::BTreeSet<Vec<u8>>,
    reader: &impl crate::callgraph_store::join::ManifestBlobReader,
) -> crate::callgraph_store::Result<InputDiff> {
    use crate::callgraph_store::join::CallgraphBlob;
    use crate::views::{ManifestEntry, RelPath};
    let mut result = InputDiff::default();
    for path in changed {
        let rel = RelPath::new(path.clone()).expect("manifest path");
        let marked = [base, next].iter().any(|manifest| {
            matches!(
                manifest.get(&rel),
                Some(ManifestEntry::Regular {
                    resolution_input: true,
                    ..
                })
            )
        });
        if !marked && !crate::callgraph_store::join::view_resolution_config(path) {
            continue;
        }
        let key = |manifest: &crate::views::Manifest| match manifest.get(&rel) {
            Some(ManifestEntry::Regular { planes, .. }) => planes.callgraph.clone(),
            _ => None,
        };
        if key(base) == key(next) && base.get(&rel).is_some() == next.get(&rel).is_some() {
            continue;
        }
        result.inputs_changed = true;
        let mut projected = Vec::new();
        for manifest in [base, next] {
            let bytes = if manifest.get(&rel).is_none() {
                Some(Vec::new())
            } else if let Some(key) = key(manifest) {
                let blob = reader
                    .read_callgraph_blob_decoded(&key)
                    .map_err(|e| {
                        crate::callgraph_store::CallGraphStoreError::Unavailable(e.to_string())
                    })?
                    .ok_or_else(|| {
                        crate::callgraph_store::CallGraphStoreError::Unavailable(format!(
                            "missing resolution input blob {key}"
                        ))
                    })?;
                match blob.as_ref() {
                    CallgraphBlob::Config(config) => Some(config.source.clone()),
                    _ => None,
                }
            } else {
                None
            };
            projected.push(bytes.and_then(|bytes| project(path, &bytes)));
        }
        let (Some(before), Some(after)) = (&projected[0], &projected[1]) else {
            result.unknown += 1;
            continue;
        };
        let Ok(path) = std::str::from_utf8(path) else {
            result.unknown += 1;
            continue;
        };
        for name in before.keys().chain(after.keys()) {
            if before.get(name) != after.get(name) {
                result.changed.insert((path.into(), name.clone()));
            }
        }
    }
    Ok(result)
}
