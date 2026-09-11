use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use aft::commands::semantic_search::extensions::{
    DefaultSearchExtensions, RawQuery, ReadinessObservation, ReadinessSource, ReadinessWait, Root,
    SearchExtensions, SemanticReadiness, SemanticSnapshot, SymbolIndexStatus, SymbolReadiness,
    TrigramReadiness,
};
use aft::commands::semantic_search::plan_table::{SearchLaneKind, SearchShape};
use aft::context::SemanticIndexStatus;
use aft::parser::SymbolCache;
use aft::search_b2::readiness::selected_lane_disclosures;
use aft::search_index::{IndexStatus, SearchIndex};
use aft::semantic_index::SemanticIndex;

struct SequenceSource {
    samples: Mutex<VecDeque<ReadinessObservation<'static>>>,
    sample_count: AtomicUsize,
    wait_count: AtomicUsize,
    wait_outcome: ReadinessWait,
}

impl SequenceSource {
    fn new(samples: impl IntoIterator<Item = ReadinessObservation<'static>>) -> Self {
        Self {
            samples: Mutex::new(samples.into_iter().collect()),
            sample_count: AtomicUsize::new(0),
            wait_count: AtomicUsize::new(0),
            wait_outcome: ReadinessWait::Completed,
        }
    }

    fn cancelled(samples: impl IntoIterator<Item = ReadinessObservation<'static>>) -> Self {
        Self {
            wait_outcome: ReadinessWait::Cancelled,
            ..Self::new(samples)
        }
    }
}

impl ReadinessSource for SequenceSource {
    fn sample(&self) -> ReadinessObservation<'_> {
        self.sample_count.fetch_add(1, Ordering::SeqCst);
        let mut samples = self.samples.lock().unwrap();
        if samples.len() > 1 {
            samples.pop_front().unwrap()
        } else {
            samples.front().unwrap().clone()
        }
    }

    fn bounded_first_search_wait(&self) -> ReadinessWait {
        self.wait_count.fetch_add(1, Ordering::SeqCst);
        self.wait_outcome
    }
}

struct GuardSource {
    semantic_index: RwLock<Option<SemanticIndex>>,
}

impl ReadinessSource for GuardSource {
    fn sample(&self) -> ReadinessObservation<'_> {
        ReadinessObservation {
            semantic: SemanticReadiness {
                status: SemanticIndexStatus::ready(),
                snapshot: Some(SemanticSnapshot::from_guard(
                    self.semantic_index.read().unwrap(),
                )),
                evicted: false,
                lock_contended: false,
            },
            trigram: trigram(IndexStatus::Ready),
            symbol: symbol(SymbolIndexStatus::Ready),
        }
    }

    fn bounded_first_search_wait(&self) -> ReadinessWait {
        panic!("a ready semantic snapshot must not enter the first-search wait")
    }
}

fn semantic_index_with_entries(root: &std::path::Path, count: usize) -> SemanticIndex {
    let file_count = count.div_ceil(500).max(1);
    let mut files = Vec::with_capacity(file_count);
    for file_index in 0..file_count {
        let start = file_index * 500;
        let end = (start + 500).min(count);
        let source = root.join(format!("many_{file_index}.rs"));
        let body = (start..end)
            .map(|index| {
                format!("pub fn readiness_symbol_{index}() -> usize {{\n    {index}\n}}\n")
            })
            .collect::<String>();
        std::fs::write(&source, body).unwrap();
        files.push(source);
    }
    SemanticIndex::build(
        root,
        &files,
        &mut |texts| Ok(vec![vec![1.0]; texts.len()]),
        count.max(1),
    )
    .unwrap()
}

fn semantic(status: SemanticIndexStatus) -> SemanticReadiness<'static> {
    let snapshot = matches!(status, SemanticIndexStatus::Ready { .. }).then(|| {
        let index = Box::leak(Box::new(RwLock::new(Some(SemanticIndex::new(
            std::env::temp_dir(),
            1,
        )))));
        SemanticSnapshot::from_guard(index.read().unwrap())
    });
    SemanticReadiness {
        status,
        snapshot,
        evicted: false,
        lock_contended: false,
    }
}

fn trigram(status: IndexStatus) -> TrigramReadiness {
    let snapshot = (status == IndexStatus::Ready).then(|| {
        let mut index = SearchIndex::new();
        index.ready = true;
        Arc::new(index.snapshot())
    });
    TrigramReadiness {
        status,
        snapshot,
        evicted: false,
        lock_contended: false,
    }
}

fn symbol(status: SymbolIndexStatus) -> SymbolReadiness {
    let snapshot = (status == SymbolIndexStatus::Ready).then(|| Arc::new(SymbolCache::new()));
    SymbolReadiness {
        status,
        snapshot,
        evicted: false,
        lock_contended: false,
    }
}

fn observation(
    semantic_status: SemanticIndexStatus,
    trigram_status: IndexStatus,
    symbol_status: SymbolIndexStatus,
) -> ReadinessObservation<'static> {
    ReadinessObservation {
        semantic: semantic(semantic_status),
        trigram: trigram(trigram_status),
        symbol: symbol(symbol_status),
    }
}

fn sample(
    source: &dyn ReadinessSource,
) -> aft::commands::semantic_search::extensions::Readiness<'_> {
    let root = Root::new(".", source);
    DefaultSearchExtensions.sample_readiness(&root)
}

fn reason<'a>(
    readiness: &'a aft::commands::semantic_search::extensions::Readiness<'_>,
    source: &str,
) -> &'a str {
    readiness
        .reasons
        .iter()
        .find_map(|reason| reason.strip_prefix(source))
        .unwrap()
}

#[test]
fn semantic_retention_is_zero_copy_for_ten_thousand_entries() {
    let project = tempfile::tempdir().unwrap();
    let index = semantic_index_with_entries(project.path(), 10_000);
    assert!(
        index.entry_count() >= 10_000,
        "expected at least 10,000 semantic entries, got {}",
        index.entry_count()
    );
    let source = GuardSource {
        semantic_index: RwLock::new(Some(index)),
    };

    let readiness = sample(&source);
    let retained = readiness.retained().semantic().unwrap();
    let live_guard = source.semantic_index.read().unwrap();
    let live_index = live_guard.as_ref().unwrap();
    assert!(std::ptr::eq(retained, live_index));
    drop(live_guard);
    assert!(matches!(
        source.semantic_index.try_write(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
}

#[test]
fn semantic_states_include_refreshing_ready_and_precise_failures() {
    let mut refreshing = SemanticIndexStatus::ready();
    refreshing.add_refreshing_file(PathBuf::from("src/lib.rs"));

    for (status, expected_ready, expected_reason) in [
        (SemanticIndexStatus::Disabled, false, Some("disabled")),
        (
            SemanticIndexStatus::Building {
                stage: "loading_artifacts".to_string(),
                files: Some(3),
                entries_done: Some(1),
                entries_total: Some(4),
            },
            false,
            Some("building:loading_artifacts"),
        ),
        (
            SemanticIndexStatus::Failed("dimension_mismatch".to_string()),
            false,
            Some("failed:dimension_mismatch"),
        ),
        (SemanticIndexStatus::ready(), true, None),
        (refreshing, true, None),
    ] {
        let source = SequenceSource::new([observation(
            status,
            IndexStatus::Ready,
            SymbolIndexStatus::Ready,
        )]);
        let readiness = sample(&source);
        assert_eq!(readiness.semantic_index, expected_ready);
        assert_eq!(
            readiness
                .reasons
                .iter()
                .find_map(|value| value.strip_prefix("semantic:")),
            expected_reason
        );
        assert_eq!(readiness.retained().semantic().is_some(), expected_ready);
        assert_eq!(source.wait_count.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn trigram_and_symbol_states_report_runtime_reasons() {
    for (status, expected_ready, expected_reason) in [
        (IndexStatus::Ready, true, None),
        (IndexStatus::Building, false, Some("building:trigram_index")),
        (IndexStatus::Fallback, false, Some("failed:fallback")),
        (IndexStatus::Disabled, false, Some("disabled")),
    ] {
        let source = SequenceSource::new([observation(
            SemanticIndexStatus::ready(),
            status,
            SymbolIndexStatus::Ready,
        )]);
        let readiness = sample(&source);
        assert_eq!(readiness.lexical_index, expected_ready);
        assert_eq!(
            readiness
                .reasons
                .iter()
                .find_map(|value| value.strip_prefix("trigram:")),
            expected_reason
        );
    }

    for (status, expected_reason) in [
        (SymbolIndexStatus::Building, "building:symbol_cache"),
        (SymbolIndexStatus::Disabled, "disabled"),
        (
            SymbolIndexStatus::Failed("cache_decode".to_string()),
            "failed:cache_decode",
        ),
    ] {
        let source = SequenceSource::new([observation(
            SemanticIndexStatus::ready(),
            IndexStatus::Ready,
            status,
        )]);
        let readiness = sample(&source);
        assert!(!readiness.symbol_index);
        assert_eq!(reason(&readiness, "symbol:"), expected_reason);
    }
}

#[test]
fn precedence_is_disabled_failed_evicted_contention_then_building() {
    let mut disabled_and_evicted = observation(
        SemanticIndexStatus::Disabled,
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    );
    disabled_and_evicted.semantic.evicted = true;
    disabled_and_evicted.semantic.lock_contended = true;
    let source = SequenceSource::new([disabled_and_evicted]);
    let readiness = sample(&source);
    assert_eq!(reason(&readiness, "semantic:"), "disabled");

    let mut failed_and_evicted = observation(
        SemanticIndexStatus::Failed("bad_model".to_string()),
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    );
    failed_and_evicted.semantic.evicted = true;
    failed_and_evicted.semantic.lock_contended = true;
    let source = SequenceSource::new([failed_and_evicted]);
    let readiness = sample(&source);
    assert_eq!(reason(&readiness, "semantic:"), "failed:bad_model");

    let mut evicted_and_contended = observation(
        SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        },
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    );
    evicted_and_contended.semantic.evicted = true;
    evicted_and_contended.semantic.lock_contended = true;
    let source = SequenceSource::new([evicted_and_contended]);
    let readiness = sample(&source);
    assert_eq!(reason(&readiness, "semantic:"), "evicted");

    let mut contended_and_building = observation(
        SemanticIndexStatus::Building {
            stage: "loading_artifacts".to_string(),
            files: None,
            entries_done: None,
            entries_total: None,
        },
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    );
    contended_and_building.semantic.lock_contended = true;
    let source = SequenceSource::new([contended_and_building]);
    let readiness = sample(&source);
    assert_eq!(reason(&readiness, "semantic:"), "lock_contention");
}

#[test]
fn eviction_and_lock_contention_are_distinct_for_every_source() {
    let mut semantic_evicted = observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    );
    semantic_evicted.semantic.snapshot = None;
    semantic_evicted.semantic.evicted = true;
    assert_eq!(
        reason(
            &sample(&SequenceSource::new([semantic_evicted])),
            "semantic:"
        ),
        "evicted"
    );

    let mut trigram_contended = observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Building,
        SymbolIndexStatus::Ready,
    );
    trigram_contended.trigram.lock_contended = true;
    assert_eq!(
        reason(
            &sample(&SequenceSource::new([trigram_contended])),
            "trigram:"
        ),
        "lock_contention"
    );

    let mut symbol_evicted = observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Ready,
        SymbolIndexStatus::Building,
    );
    symbol_evicted.symbol.evicted = true;
    symbol_evicted.symbol.lock_contended = true;
    assert_eq!(
        reason(&sample(&SequenceSource::new([symbol_evicted])), "symbol:"),
        "evicted"
    );

    let mut trigram_disabled_evicted = observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Disabled,
        SymbolIndexStatus::Ready,
    );
    trigram_disabled_evicted.trigram.evicted = true;
    assert_eq!(
        reason(
            &sample(&SequenceSource::new([trigram_disabled_evicted])),
            "trigram:"
        ),
        "disabled"
    );
}

#[test]
fn transition_during_wait_changes_the_selected_plan() {
    let source = SequenceSource::new([
        observation(
            SemanticIndexStatus::Disabled,
            IndexStatus::Disabled,
            SymbolIndexStatus::Disabled,
        ),
        observation(
            SemanticIndexStatus::ready(),
            IndexStatus::Building,
            SymbolIndexStatus::Disabled,
        ),
    ]);

    let readiness = sample(&source);
    assert!(readiness.semantic_index);
    assert!(!readiness.lexical_index);
    assert_eq!(reason(&readiness, "trigram:"), "building:trigram_index");
    let raw_query = RawQuery::new("where does semantic readiness change the plan");
    let (shape, facts) = DefaultSearchExtensions.classify(&raw_query);
    assert_eq!(shape, SearchShape::NaturalLanguage);
    let plan = DefaultSearchExtensions.plan(&shape, &facts, &readiness);
    assert!(plan.contains(SearchLaneKind::Semantic));
    assert!(!plan.contains(SearchLaneKind::Lexical));
    assert_eq!(source.sample_count.load(Ordering::SeqCst), 2);
    assert_eq!(source.wait_count.load(Ordering::SeqCst), 1);
}

#[test]
fn one_ready_source_skips_wait_and_second_sample() {
    let source = SequenceSource::new([
        observation(
            SemanticIndexStatus::Disabled,
            IndexStatus::Ready,
            SymbolIndexStatus::Disabled,
        ),
        observation(
            SemanticIndexStatus::ready(),
            IndexStatus::Disabled,
            SymbolIndexStatus::Ready,
        ),
    ]);

    let readiness = sample(&source);
    assert!(readiness.lexical_index);
    assert!(!readiness.semantic_index);
    assert_eq!(source.sample_count.load(Ordering::SeqCst), 1);
    assert_eq!(source.wait_count.load(Ordering::SeqCst), 0);
}

#[test]
fn cancellation_during_the_bounded_wait_is_preserved() {
    let source = SequenceSource::cancelled([observation(
        SemanticIndexStatus::Disabled,
        IndexStatus::Disabled,
        SymbolIndexStatus::Disabled,
    )]);
    let readiness = sample(&source);
    assert!(readiness.cancelled());
    assert_eq!(source.sample_count.load(Ordering::SeqCst), 1);
    assert_eq!(source.wait_count.load(Ordering::SeqCst), 1);
}

#[test]
fn selected_snapshots_survive_source_eviction_without_a_footer() {
    let project = tempfile::tempdir().unwrap();
    let source = GuardSource {
        semantic_index: RwLock::new(Some(semantic_index_with_entries(project.path(), 1))),
    };

    let readiness = sample(&source);
    assert!(matches!(
        source.semantic_index.try_write(),
        Err(std::sync::TryLockError::WouldBlock)
    ));
    assert_eq!(
        readiness
            .retained()
            .semantic()
            .unwrap()
            .search(&[1.0], 1)
            .len(),
        1
    );
    assert!(readiness.retained().trigram().is_some());
    assert!(readiness.retained().symbol().is_some());
    assert!(selected_lane_disclosures(
        &[
            SearchLaneKind::Symbol,
            SearchLaneKind::Lexical,
            SearchLaneKind::Semantic,
        ],
        &readiness,
        &[],
        None,
    )
    .is_empty());

    drop(readiness);
    assert!(source.semantic_index.try_write().is_ok());
}

#[test]
fn selected_lane_disclosure_is_literal_ordered_and_source_specific() {
    let path_source = SequenceSource::new([observation(
        SemanticIndexStatus::Disabled,
        IndexStatus::Ready,
        SymbolIndexStatus::Ready,
    )]);
    assert!(selected_lane_disclosures(
        &[SearchLaneKind::PathLookup, SearchLaneKind::Lexical],
        &sample(&path_source),
        &[],
        None,
    )
    .is_empty());

    let identifier_source = SequenceSource::new([observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Ready,
        SymbolIndexStatus::Disabled,
    )]);
    assert_eq!(
        selected_lane_disclosures(
            &[
                SearchLaneKind::Symbol,
                SearchLaneKind::Lexical,
                SearchLaneKind::Variants,
            ],
            &sample(&identifier_source),
            &[],
            None,
        ),
        ["symbol cache disabled; definition-first skipped"]
    );

    let ordered_source = SequenceSource::new([observation(
        SemanticIndexStatus::Disabled,
        IndexStatus::Building,
        SymbolIndexStatus::Disabled,
    )]);
    assert_eq!(
        selected_lane_disclosures(
            &[
                SearchLaneKind::Symbol,
                SearchLaneKind::Lexical,
                SearchLaneKind::Variants,
                SearchLaneKind::Semantic,
                SearchLaneKind::FallbackWalk,
            ],
            &sample(&ordered_source),
            &["parseHeader".to_string()],
            Some(7),
        ),
        [
            "trigram index building:trigram_index; lexical lane skipped",
            "symbol cache disabled; definition-first skipped",
            "semantic index disabled; semantic lane skipped",
            "variants applied: parseHeader",
            "fallback walk: 7 files scanned",
        ]
    );
}

#[test]
fn trigram_building_lexical_disclosure_is_not_silent() {
    let source = SequenceSource::new([observation(
        SemanticIndexStatus::ready(),
        IndexStatus::Building,
        SymbolIndexStatus::Ready,
    )]);
    let lines = selected_lane_disclosures(
        &[SearchLaneKind::Lexical, SearchLaneKind::Variants],
        &sample(&source),
        &[],
        None,
    );
    assert_eq!(
        lines,
        ["trigram index building:trigram_index; lexical lane skipped"]
    );
}

#[test]
fn stale_index_reachability_fixture_is_an_independent_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/search_b2/readiness/index_stale_reachability.json"
    ))
    .unwrap();
    let rows = fixture["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["episode"], 4173);
    assert_eq!(rows[0]["expectations"]["111"], "reachable_via:lexical");
    assert_eq!(rows[1]["expectations"]["001"], "fallback_walk");
    assert_eq!(rows[2]["expectations"]["000"], "fallback_walk");
    assert!(rows.iter().all(|row| row.get("lanes_run").is_none()));
    assert_eq!(fixture["b1_gate"], "unchanged");
}
