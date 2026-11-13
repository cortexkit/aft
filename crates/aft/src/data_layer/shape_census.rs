//! Opt-in, bounded method-call shape measurements for cold builds.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;

use super::query::SliceSource;

use crate::callgraph_store::join::dispatch::{Resolution, SiteHint};

#[derive(Default)]
pub(crate) struct Census {
    groups: BTreeMap<(String, String), (u64, usize)>,
    languages: BTreeMap<String, BTreeMap<String, u64>>,
}

aft_store::heap_bytes_of_fields!(Census { groups, languages });

impl Census {
    #[cfg(test)]
    pub(crate) fn into_json(self) -> serde_json::Value {
        serde_json::to_value(self.document(Vec::new())).unwrap()
    }

    pub(crate) fn record(
        &mut self,
        language: &str,
        site: &SiteHint,
        exit: &str,
        answer: &Resolution,
    ) {
        assert!(!exit.is_empty(), "every resolver exit must be classified");
        *self
            .languages
            .entry(language.to_string())
            .or_default()
            .entry(exit.to_string())
            .or_default() += 1;
        if exit.starts_with("unknown") {
            let member = site.member.as_ref().expect("unknown has a member");
            let group = self
                .groups
                .entry((language.to_string(), member.clone()))
                .or_insert((0, answer.targets.len()));
            assert_eq!(
                group.1,
                answer.targets.len(),
                "unknown answer depends only on language and member"
            );
            group.0 += 1;
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        for (key, (sites, targets)) in other.groups {
            let group = self.groups.entry(key).or_insert((0, targets));
            assert_eq!(group.1, targets);
            group.0 += sites;
        }
        for (language, exits) in other.languages {
            let total = self.languages.entry(language).or_default();
            for (exit, count) in exits {
                *total.entry(exit).or_default() += count;
            }
        }
    }

    // JSON schema version 1:
    // {schema_version: 1, groups: [{language, member, sites, targets}],
    //  language_totals: [{language, sites, exits: {exit_name: sites}}],
    //  top_callers: [{path, symbol, callers}]}.
    // Counts are exact unsigned integers. `targets` counts explicit targets,
    // not same-named declarations merely kept live without a call edge.
    // Groups cover all unknown() routes; sum the
    // exits named `unknown*` to get their per-language share of all bound method
    // sites (including dynamic, missing-member and ordinary-edge-preserved sites).
    // Exits: dynamic, missing_member, unknown_go_package, unknown,
    // rust_template_self, cpp_type_match, python_super, missing_type,
    // rust_legacy_shape, unknown_rust_ambiguous, unknown_go_ambiguous, open_type,
    // exact. An exact/open_type route may return zero targets. Absent exit keys
    // mean zero. Groups/languages sort lexically; top callers sort by descending
    // count then path/symbol, include tests and callback references, and dedup
    // caller declarations by file and source position, not call sites or lines.
    // Paths are root-relative.
    pub(crate) fn write_if_requested(self, source: &dyn SliceSource) -> Result<(), String> {
        let Some(path) = std::env::var_os("AFT_CALLGRAPH_SHAPE_CENSUS_JSON").map(PathBuf::from)
        else {
            return Ok(());
        };
        if !path.is_absolute() {
            return Err("AFT_CALLGRAPH_SHAPE_CENSUS_JSON must be an absolute path".into());
        }
        let document = self.document(top_callers(source)?);
        let bytes = serde_json::to_vec_pretty(&document).map_err(|error| error.to_string())?;
        std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn document(self, top_callers: Vec<TopCaller>) -> Document {
        Document {
            schema_version: 1,
            groups: self
                .groups
                .into_iter()
                .map(|((language, member), (sites, targets))| Group {
                    language,
                    member,
                    sites,
                    targets,
                })
                .collect(),
            language_totals: self
                .languages
                .into_iter()
                .map(|(language, exits)| LanguageTotal {
                    language,
                    sites: exits.values().sum(),
                    exits,
                })
                .collect(),
            top_callers,
        }
    }
}

#[derive(Serialize)]
struct Document {
    schema_version: u32,
    groups: Vec<Group>,
    language_totals: Vec<LanguageTotal>,
    top_callers: Vec<TopCaller>,
}

#[derive(Serialize)]
struct Group {
    language: String,
    member: String,
    sites: u64,
    targets: usize,
}

#[derive(Serialize)]
struct LanguageTotal {
    language: String,
    sites: u64,
    exits: BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct TopCaller {
    path: String,
    symbol: String,
    callers: u64,
}

fn top_callers(source: &dyn SliceSource) -> Result<Vec<TopCaller>, String> {
    let mut counts = BTreeMap::<(String, String), u64>::new();
    for file in source.files() {
        let slice = source
            .slice(&file)
            .ok_or_else(|| format!("missing census slice: {file}"))?;
        for node in slice.nodes() {
            counts
                .entry((file.clone(), node.scoped_name().to_string()))
                .or_default();
        }
        // Duplicate sites or edge records from the same caller contribute once.
        // Retain only distinct caller/target groups in this file, never sites;
        // the cross-file map holds just one counter per target symbol.
        let mut pairs = BTreeSet::new();
        for edge in slice.edges() {
            if slice.node(edge.source()).is_none() || edge.reference().is_none() {
                continue;
            }
            let callable = match edge.kind() {
                "call" | "callback_ref" => true,
                "construction" => {
                    let target = source
                        .slice(edge.target_file())
                        .ok_or_else(|| format!("missing census target: {}", edge.target_file()))?;
                    target
                        .target_node(edge.target_symbol(), edge.lookup())
                        .and_then(|handle| target.node_kind(handle))
                        .is_some_and(|kind| matches!(kind, "function" | "method"))
                }
                _ => false,
            };
            if callable {
                pairs.insert((edge.source(), edge.target_file(), edge.target_symbol()));
            }
        }
        for (_, path, symbol) in pairs {
            *counts
                .entry((path.to_string(), symbol.to_string()))
                .or_default() += 1;
        }
    }
    let mut top = Vec::new();
    for ((path, symbol), callers) in counts {
        top.push(TopCaller {
            path,
            symbol,
            callers,
        });
        top.sort_by(|a, b| {
            b.callers
                .cmp(&a.callers)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        top.truncate(10);
    }
    Ok(top)
}
