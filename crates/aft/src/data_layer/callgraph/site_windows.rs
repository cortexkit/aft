//! Resolves and encodes incremental readers without changing the resolver's input map.

use super::{
    CheckoutInputs, EncodedResolution, FileResolution, MethodCallInputs, ResolutionSlot,
    ResolutionSource, Resolutions, SiteResolution, BATCH,
};
use crate::callgraph_store::join::ManifestJoinError;
use crate::data_layer::incremental::UpdateError;
use crate::data_layer::refresh_memory::{RefreshChecks, RefreshMemory};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

pub(in crate::data_layer) const READER_RESOLVE_WINDOW: usize = BATCH;
const READER_ENCODE_BATCH: usize = 32;

pub(in crate::data_layer) type EncodedSites = Vec<(String, EncodedResolution, BTreeSet<String>)>;

fn window_len<T>(remaining: &[T], budget: usize, estimate: impl Fn(&T) -> usize) -> usize {
    let Some((first, rest)) = remaining.split_first() else {
        return 0;
    };
    // A single oversized reader still has to run, but it gets its own window.
    let mut available = budget.saturating_sub(estimate(first));
    let mut len = 1;
    for reader in rest {
        if len == READER_RESOLVE_WINDOW || available == 0 {
            break;
        }
        let Some(left) = available.checked_sub(estimate(reader)) else {
            break;
        };
        available = left;
        len += 1;
    }
    len
}

fn estimate(inputs: &CheckoutInputs, path: &str) -> usize {
    // Source length is a cheap proxy for the number of sites: estimating must
    // not decode every reader. Charge 16 KiB per 128 source bytes to allow for
    // dispatch answers that contain many targets. This is not a heap limit;
    // the refresh guard still measures growth inside the resolver.
    inputs
        .state
        .get(path.as_bytes())
        .map_or(usize::MAX, |record| {
            usize::try_from(record.stat.size)
                .unwrap_or(usize::MAX)
                .max(1)
                .saturating_mul(128)
        })
}

impl MethodCallInputs {
    pub(in crate::data_layer) fn resolve_sites_encoded(
        &mut self,
        inputs: &CheckoutInputs,
        resolved: &Resolutions,
        mut paths: &[&String],
        guard: Option<&RefreshMemory>,
        #[cfg(test)] decoded_high_water: &mut usize,
    ) -> Result<EncodedSites, UpdateError> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let budget = crate::memory::profile::current().bind_batch_bytes;
        self.resolve_site_pass(inputs, resolved, paths, guard, |source, projects, chunk| {
            let mut encoded = Vec::with_capacity(paths.len());
            while !paths.is_empty() {
                let len = window_len(paths, budget, |path| estimate(inputs, path));
                let (window, rest) = paths.split_at(len);
                paths = rest;
                let results = source.resolve_window(window, projects, chunk, guard)?;
                #[cfg(test)]
                {
                    // Each reader's results stay decoded until encoding starts.
                    *decoded_high_water = (*decoded_high_water).max(results.len());
                }
                encode_window(inputs, resolved, results, &mut encoded, guard)?;
            }
            Ok(encoded)
        })
    }
}

fn encode_window(
    inputs: &CheckoutInputs,
    resolved: &Resolutions,
    mut results: Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>,
    encoded: &mut EncodedSites,
    guard: Option<&RefreshMemory>,
) -> Result<(), UpdateError> {
    let carry = crate::alloc_trace::carry();
    for batch in results.chunks_mut(READER_ENCODE_BATCH) {
        let batch = batch
            .par_iter_mut()
            .map(|result| {
                guard.file()?;
                let _label = carry.enter();
                let (path, sites, keys) = std::mem::take(result);
                let key = inputs
                    .parses
                    .key(&path)
                    .ok_or_else(|| ManifestJoinError::Parse(format!("missing parse for {path}")))?;
                // The map must remain readable for later readers. Decode
                // without caching and don't copy the sites we're replacing.
                let previous = resolved.read(&path).ok_or_else(|| {
                    ManifestJoinError::Parse(format!(
                        "stored resolution of {path} could not be read"
                    ))
                })?;
                let mut file =
                    Arc::try_unwrap(previous).unwrap_or_else(|previous| FileResolution {
                        rows: previous.rows.clone(),
                        binding: previous.binding.clone(),
                        dispatch: previous.dispatch.clone(),
                        ..FileResolution::default()
                    });
                file.sites = sites;
                file.site_dependencies = keys;
                let encoded = EncodedResolution::encode_checked(&key, &file, guard)?;
                Ok((path, encoded, file.site_dependencies))
            })
            .collect::<Result<Vec<_>, UpdateError>>()?;
        encoded.extend(batch);
    }
    Ok(())
}

impl Resolutions {
    /// Install only after every reader has resolved against the original map.
    /// If the guard stops installation, the caller must discard the live
    /// checkout engine: only some readers' slots and reverse keys were replaced.
    pub(in crate::data_layer) fn install_sites_checked(
        &mut self,
        encoded: EncodedSites,
        mut note_sites: impl FnMut(&str, &BTreeSet<String>),
        guard: Option<&RefreshMemory>,
    ) -> Result<(), UpdateError> {
        for (path, encoded, keys) in encoded {
            guard.file()?;
            self.changed.remove(&path);
            self.map.insert(
                path.clone(),
                ResolutionSlot {
                    value: OnceLock::new(),
                    source: ResolutionSource::Encoded(Arc::new(encoded)),
                },
            );
            note_sites(&path, &keys);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_window_respects_both_budgets_and_advances_past_large_files() {
        let sizes = [4, 6, 3, usize::MAX, 1];
        assert_eq!(window_len(&sizes, 10, |size| *size), 2);
        assert_eq!(window_len(&sizes[2..], 10, |size| *size), 1);
        assert_eq!(window_len(&sizes[3..], 10, |size| *size), 1);
        assert_eq!(window_len(&sizes[4..], 0, |size| *size), 1);
        let many = [1; READER_RESOLVE_WINDOW + 1];
        assert_eq!(
            window_len(&many, usize::MAX, |size| *size),
            READER_RESOLVE_WINDOW
        );
    }
}
