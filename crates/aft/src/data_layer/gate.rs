//! Compares two call-graph readers lookup by lookup, for the gates that
//! check an incrementally updated or restarted checkout against a cold build
//! of the same files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

/// Runs every store primitive the commands use on `ours` and on `theirs`,
/// for a sample of symbols and names, and records each primitive's calls,
/// differences and time in `queries`.
pub(crate) fn compare_readers(
    ours: &impl crate::callgraph_store::CallGraphRead,
    theirs: &impl crate::callgraph_store::CallGraphRead,
    inputs: &super::callgraph::CheckoutInputs,
    queries: &mut BTreeMap<&'static str, (usize, usize, f64, f64)>,
    examples_out: &mut Vec<String>,
) {
    use crate::parser::LangId;

    let mut examples = std::mem::take(examples_out);
    let mut compare = |name: &'static str,
                       ours_run: &mut dyn FnMut() -> String,
                       theirs_run: &mut dyn FnMut() -> String,
                       examples: &mut Vec<String>| {
        let started = Instant::now();
        let left = ours_run();
        let ours_ms = started.elapsed().as_secs_f64() * 1000.0;
        let started = Instant::now();
        let right = theirs_run();
        let theirs_ms = started.elapsed().as_secs_f64() * 1000.0;
        let entry = queries.entry(name).or_default();
        entry.0 += 1;
        entry.2 += ours_ms;
        entry.3 += theirs_ms;
        if left != right {
            entry.1 += 1;
            if examples.len() < 60 && entry.1 <= 3 {
                let left_lines = left.lines().collect::<BTreeSet<_>>();
                let right_lines = right.lines().collect::<BTreeSet<_>>();
                examples.push(format!("query {name} differs"));
                for line in left_lines.difference(&right_lines).take(4) {
                    examples.push(format!("  slices: {}", line.trim()));
                }
                for line in right_lines.difference(&left_lines).take(4) {
                    examples.push(format!("  views:  {}", line.trim()));
                }
            }
        }
    };
    let show = |value: &dyn std::fmt::Debug| without_entry_facts(&format!("{value:#?}"));

    // A deterministic sample of symbols: every 20th node in path order.
    let mut symbols = Vec::new();
    for (path, (_, parse)) in &inputs.parses {
        for symbol in &parse.symbols {
            symbols.push((
                path.clone(),
                symbol.scoped_name.clone(),
                symbol.name.clone(),
            ));
        }
    }
    let sample = symbols
        .iter()
        .step_by(20 * sample_scale())
        .cloned()
        .collect::<Vec<_>>();
    for chunk in sample.chunks(200) {
        let targets = chunk
            .iter()
            .map(|(file, scoped, _)| (file.clone(), scoped.clone()))
            .collect::<Vec<_>>();
        let sorted = |map: std::collections::HashMap<
            (String, String),
            Vec<crate::callgraph_store::StoreCallSite>,
        >| { map.into_iter().collect::<BTreeMap<_, _>>() };
        compare(
            "direct_callers_for_symbols",
            &mut || {
                show(
                    &ours
                        .direct_callers_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .direct_callers_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "direct_caller_counts_of",
            &mut || {
                show(
                    &ours
                        .direct_caller_counts_of(&targets)
                        .map(|m| m.into_iter().collect::<BTreeMap<_, _>>())
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .direct_caller_counts_of(&targets)
                        .map(|m| m.into_iter().collect::<BTreeMap<_, _>>())
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "outgoing_calls_for_symbols",
            &mut || {
                show(
                    &ours
                        .outgoing_calls_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .outgoing_calls_for_symbols(&targets)
                        .map(sorted)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
    }
    for (file, scoped, name) in &sample {
        for query in [scoped, name] {
            compare(
                "nodes_for_stored",
                &mut || {
                    show(
                        &ours
                            .nodes_for_stored(file, query)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .nodes_for_stored(file, query)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
        compare(
            "nodes_matching",
            &mut || show(&ours.nodes_matching(name).map_err(|e| e.to_string())),
            &mut || show(&theirs.nodes_matching(name).map_err(|e| e.to_string())),
            &mut examples,
        );
        compare(
            "trace_to_symbol_candidates",
            &mut || {
                show(
                    &ours
                        .trace_to_symbol_candidates(scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .trace_to_symbol_candidates(scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "node_for",
            &mut || {
                show(
                    &ours
                        .node_for(Path::new(file), scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .node_for(Path::new(file), scoped)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        compare(
            "stored_content_hash",
            &mut || show(&ours.stored_content_hash(file).map_err(|e| e.to_string())),
            &mut || show(&theirs.stored_content_hash(file).map_err(|e| e.to_string())),
            &mut examples,
        );
        let Ok(nodes) = theirs.nodes_for_stored(file, scoped) else {
            continue;
        };
        for node in &nodes {
            compare(
                "outgoing_calls_of",
                &mut || show(&ours.outgoing_calls_of(node).map_err(|e| e.to_string())),
                &mut || show(&theirs.outgoing_calls_of(node).map_err(|e| e.to_string())),
                &mut examples,
            );
            compare(
                "resolved_self_calls_of",
                &mut || show(&ours.resolved_self_calls_of(node).map_err(|e| e.to_string())),
                &mut || {
                    show(
                        &theirs
                            .resolved_self_calls_of(node)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
            compare(
                "unresolved_calls_of",
                &mut || show(&ours.unresolved_calls_of(node).map_err(|e| e.to_string())),
                &mut || show(&theirs.unresolved_calls_of(node).map_err(|e| e.to_string())),
                &mut examples,
            );
        }
    }

    // Names used by references: every 50th distinct short name.
    let mut names = BTreeSet::new();
    for (_, parse) in inputs.parses.values() {
        for reference in &parse.refs {
            if let Some(name) = &reference.short_name {
                names.insert(name.clone());
            }
        }
    }
    let languages = [
        LangId::Rust,
        LangId::TypeScript,
        LangId::JavaScript,
        LangId::Python,
        LangId::Go,
    ];
    for name in names.iter().step_by(50 * sample_scale()) {
        for kind in ["call", "construction", "value_ref"] {
            compare(
                "ref_sites_named",
                &mut || {
                    show(
                        &ours
                            .ref_sites_named(kind, name, 50)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .ref_sites_named(kind, name, 50)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
        for language in languages {
            compare(
                "unresolved_method_sites_named",
                &mut || {
                    show(
                        &ours
                            .unresolved_method_sites_named(name, language)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .unresolved_method_sites_named(name, language)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
            for include_tests in [false, true] {
                compare(
                    "unresolved_call_site_count_named",
                    &mut || {
                        show(
                            &ours
                                .unresolved_call_site_count_named(name, language, include_tests)
                                .map_err(|e| e.to_string()),
                        )
                    },
                    &mut || {
                        show(
                            &theirs
                                .unresolved_call_site_count_named(name, language, include_tests)
                                .map_err(|e| e.to_string()),
                        )
                    },
                    &mut examples,
                );
            }
        }
    }
    for language in languages {
        compare(
            "unresolved_method_sites_named (all)",
            &mut || {
                show(
                    &ours
                        .unresolved_method_sites_named("", language)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut || {
                show(
                    &theirs
                        .unresolved_method_sites_named("", language)
                        .map_err(|e| e.to_string()),
                )
            },
            &mut examples,
        );
        for include_tests in [false, true] {
            compare(
                "unresolved_call_site_count",
                &mut || {
                    show(
                        &ours
                            .unresolved_call_site_count(language, include_tests)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut || {
                    show(
                        &theirs
                            .unresolved_call_site_count(language, include_tests)
                            .map_err(|e| e.to_string()),
                    )
                },
                &mut examples,
            );
        }
    }
    compare(
        "indexed_file_count",
        &mut || show(&ours.indexed_file_count().map_err(|e| e.to_string())),
        &mut || show(&theirs.indexed_file_count().map_err(|e| e.to_string())),
        &mut examples,
    );
    // Every edge, one per line, so a difference names the edges that differ
    // rather than only a count.
    let edges = |snapshot: crate::callgraph_store::Result<
        BTreeSet<crate::callgraph_store::StoredEdge>,
    >| match snapshot {
        Ok(edges) => edges
            .iter()
            .map(|edge| format!("{edge:?}"))
            .collect::<Vec<_>>()
            .join("\n"),
        Err(error) => format!("error: {error}"),
    };
    compare(
        "edge_snapshot",
        &mut || edges(ours.edge_snapshot()),
        &mut || edges(theirs.edge_snapshot()),
        &mut examples,
    );

    // The public operations, as the commands compose them from the
    // primitives above: what an agent's call-graph reply is built from.
    // `impact` and `trace_to` are left out: they label the roots they reach
    // from entry facts (`EntryFacts`) that only the data layer records, so
    // over the views database built here they label those roots differently.
    // Every 400th symbol in path order; each operation runs at the depth
    // the commands default to and with tests both left out and included.
    use crate::commands::callgraph_store_adapter as adapter;
    let public = symbols
        .iter()
        .step_by(400 * sample_scale())
        .cloned()
        .collect::<Vec<_>>();
    for (index, (file, scoped, _)) in public.iter().enumerate() {
        let path = Path::new(file);
        for include_tests in [false, true] {
            compare(
                "callers (public)",
                &mut || {
                    show(&adapter::callers_result(
                        ours,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::callers_result(
                        theirs,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut examples,
            );
            compare(
                "call_tree (public)",
                &mut || {
                    show(&adapter::call_tree_result(
                        ours,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut || {
                    show(&adapter::call_tree_result(
                        theirs,
                        path,
                        scoped,
                        2,
                        include_tests,
                    ))
                },
                &mut examples,
            );
        }
        // From this symbol to the next sampled one, named with its file.
        if let Some((to_file, to_scoped, _)) = public.get(index + 1) {
            let to_path = Path::new(to_file);
            compare(
                "trace_to_symbol (public)",
                &mut || {
                    show(&adapter::trace_to_symbol_result(
                        ours,
                        path,
                        scoped,
                        to_scoped,
                        Some(to_path),
                        4,
                        false,
                    ))
                },
                &mut || {
                    show(&adapter::trace_to_symbol_result(
                        theirs,
                        path,
                        scoped,
                        to_scoped,
                        Some(to_path),
                        4,
                        false,
                    ))
                },
                &mut examples,
            );
        }
    }
    *examples_out = examples;
}

/// `text`, a value's pretty `Debug` form, without the `entry` facts
/// (`EntryFacts`) of its call-graph nodes. Only the data layer's graph
/// records them; nodes read from the views database built here carry the
/// defaults, so comparing them would flag every node.
fn without_entry_facts(text: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping: Option<usize> = None;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        if let Some(open) = skipping {
            if indent == open && line.trim_start().starts_with('}') {
                skipping = None;
            }
            continue;
        }
        if line.trim_start().starts_with("entry: EntryFacts {") {
            skipping = Some(indent);
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// How much more sparsely than the default the query comparison samples.
fn sample_scale() -> usize {
    std::env::var("GATE_SAMPLE_SCALE")
        .ok()
        .and_then(|scale| scale.parse::<usize>().ok())
        .filter(|scale| *scale > 0)
        .unwrap_or(1)
}
