//! Resolving a checkout by reusing resolutions made in other checkouts.
//!
//! A file's resolution is stored with every checkout read and surface query
//! it made, each with its answer. In another checkout holding the same
//! content at the same path, the stored resolution is reused exactly when
//! binding gives the same reference bindings there and every recorded
//! answer still holds; otherwise the file is resolved again. Method-call
//! sites (pass 4) read other files' facts without recording answers yet, so
//! they are resolved again for every file.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::time::Instant;

use rayon::prelude::*;

use super::callgraph::{
    resolve_files, resolver_index, thread_facts, BindCache, CheckoutFacts, CheckoutInputs,
    FileResolution, MethodCallInputs,
};
use crate::callgraph_store::fact_answers::{FactAnswer, FactQuestion};
use crate::callgraph_store::join::{ManifestJoinError, ViewBindingDependencies, ViewSurfaceQuery};

/// Stored resolutions, by the file's parse key and path. A file can have
/// several: checkouts that resolved it differently.
pub(crate) type ReusePool<'p> = HashMap<(&'p str, &'p str), Vec<&'p FileResolution>>;

/// What resolving one checkout with reuse did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ReuseStats {
    pub files: usize,
    /// Files with at least one stored resolution.
    pub candidates: usize,
    pub reused: usize,
    pub resolved: usize,
    /// Stored resolutions rejected because binding differs here, or a
    /// recorded checkout read or surface answer does not hold here.
    pub rejected_binding: usize,
    pub rejected_reads: usize,
    pub rejected_surface: usize,
    pub bind_ms: f64,
    pub validate_ms: f64,
    pub resolve_ms: f64,
    pub sites_ms: f64,
    pub total_ms: f64,
}

/// Why a stored resolution cannot be reused here.
enum Rejection {
    Binding,
    Reads,
    Surface,
}

/// Resolves every parsed file of `inputs`, reusing what `pool` holds where
/// its recorded answers hold here.
///
/// # Errors
///
/// When the resolver fails on a file.
pub(crate) fn resolve_reusing(
    inputs: &CheckoutInputs,
    pool: &ReusePool<'_>,
    threads: usize,
) -> Result<(BTreeMap<String, FileResolution>, ReuseStats), ManifestJoinError> {
    let started = Instant::now();
    super::callgraph::pool(threads)?.install(|| {
        let mut stats = ReuseStats {
            files: inputs.parses.len(),
            ..ReuseStats::default()
        };
        let paths = inputs.parses.keys().collect::<Vec<_>>();
        let phase = Instant::now();
        let bound = BindCache::default();
        bound.bind_missing(inputs, &paths)?;
        stats.bind_ms = phase.elapsed().as_secs_f64() * 1000.0;

        let phase = Instant::now();
        let checked = paths
            .par_iter()
            .map_init(
                || {
                    let facts = Rc::new(thread_facts(inputs));
                    let (index, _files) = resolver_index(inputs, &bound, &facts);
                    (
                        facts,
                        index,
                        HashMap::new(),
                        BTreeMap::new(),
                        BTreeMap::new(),
                    )
                },
                |(_, index, reads, surface, traced), path| {
                    let key = inputs.parses[path.as_str()].0.as_str();
                    let Some(variants) = pool.get(&(key, path.as_str())) else {
                        return ((*path).clone(), None, Vec::new());
                    };
                    let own = bound.get(path).expect("every parsed file was bound");
                    let bound = &own.binding;
                    let checkout = CheckoutFacts { inputs };
                    let mut rejections = Vec::new();
                    for variant in variants {
                        match holds(variant, bound, &checkout, index, reads, surface, traced) {
                            Ok(()) => {
                                let mut binding = bound.clone();
                                binding.reuse_resolution(&variant.binding);
                                // As a resolution built in memory: no
                                // surface (see `callgraph::resolve_files`).
                                binding.clear_surface();
                                let reused = FileResolution {
                                    rows: variant.rows.clone(),
                                    binding,
                                    dispatch: variant.dispatch.clone(),
                                    ..FileResolution::default()
                                };
                                return ((*path).clone(), Some(reused), rejections);
                            }
                            Err(rejection) => rejections.push(rejection),
                        }
                    }
                    ((*path).clone(), None, rejections)
                },
            )
            .collect::<Vec<_>>();
        stats.validate_ms = phase.elapsed().as_secs_f64() * 1000.0;

        let mut resolved = BTreeMap::new();
        let mut fresh = Vec::new();
        for (path, reused, rejections) in checked {
            if pool.contains_key(&(inputs.parses[&path].0.as_str(), path.as_str())) {
                stats.candidates += 1;
            }
            for rejection in rejections {
                match rejection {
                    Rejection::Binding => stats.rejected_binding += 1,
                    Rejection::Reads => stats.rejected_reads += 1,
                    Rejection::Surface => stats.rejected_surface += 1,
                }
            }
            match reused {
                Some(file) => {
                    stats.reused += 1;
                    resolved.insert(path, file);
                }
                None => fresh.push(path),
            }
        }

        let phase = Instant::now();
        let fresh_refs = fresh.iter().collect::<Vec<_>>();
        stats.resolved = fresh.len();
        resolved.extend(resolve_files(inputs, &bound, &fresh_refs)?);
        stats.resolve_ms = phase.elapsed().as_secs_f64() * 1000.0;

        let phase = Instant::now();
        let mut calls = MethodCallInputs::build(inputs, &resolved);
        for (path, sites, dependencies) in calls.resolve_sites(inputs, &resolved, &paths) {
            let file = resolved.get_mut(&path).expect("every file is resolved");
            file.sites = sites;
            file.site_dependencies = dependencies;
        }
        stats.sites_ms = phase.elapsed().as_secs_f64() * 1000.0;
        stats.total_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok((resolved, stats))
    })
}

/// Whether `variant` can be reused for a file bound here as `bound`.
fn holds<'v>(
    variant: &'v FileResolution,
    bound: &ViewBindingDependencies,
    checkout: &CheckoutFacts<'_>,
    index: &impl crate::callgraph_store::ResolverIndex,
    reads: &mut HashMap<&'v FactQuestion, FactAnswer>,
    surface: &mut BTreeMap<&'v ViewSurfaceQuery, String>,
    traced: &mut BTreeMap<&'v ViewSurfaceQuery, String>,
) -> Result<(), Rejection> {
    // Rows follow from binding, which is the same here exactly when every
    // checkout read it made got the same answer (binding is a function of
    // the file's parse and those answers).
    if variant.binding.binding_answers != bound.binding_answers
        || variant.binding.references != bound.references
    {
        return Err(Rejection::Binding);
    }
    // A configuration read attributed to no field was not recorded with its
    // answer, so it cannot be checked here.
    if variant.binding.unattributed {
        return Err(Rejection::Reads);
    }
    let mut answers = variant.binding.resolution_answers.iter().chain(
        variant
            .dispatch
            .iter()
            .flat_map(|dispatch| dispatch.trace.answers.iter()),
    );
    if !answers.all(|(question, recorded)| {
        reads
            .entry(question)
            .or_insert_with(|| question.answer(checkout))
            == recorded
    }) {
        return Err(Rejection::Reads);
    }
    let surface_holds = variant.binding.all_surface_answers_hold(index, surface)
        && variant
            .dispatch
            .as_ref()
            .is_none_or(|dispatch| dispatch.trace.all_answers_hold(index, traced));
    if !surface_holds {
        return Err(Rejection::Surface);
    }
    Ok(())
}
