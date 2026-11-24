use super::*;

fn published_graph_sections(checkout: &Checkout) -> BTreeMap<(u16, u16), Vec<u8>> {
    let view = checkout.view().unwrap();
    assert!(view.published.graph_built);
    let mut sections = BTreeMap::new();
    for segment in &view.published.segments {
        for (handle, info) in segment.sections() {
            let kind = info.id.kind.get();
            if !(0x0300..=0x0306).contains(&kind) {
                continue;
            }
            let mut bytes = vec![0; usize::try_from(info.len).unwrap()];
            segment.read(handle, 0, &mut bytes).unwrap();
            assert!(sections
                .insert((kind, info.id.key_space.get()), bytes)
                .is_none());
        }
    }
    assert!(!sections.is_empty(), "the graph must not compare empty");
    sections
}

fn assert_cold_build_log(log: &[String], root: &Path, trigger: &str) {
    let completed = log
        .iter()
        .filter(|line| line.contains("cold call-graph build completed:"))
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1, "{log:?}");
    let prefix = format!(
        "INFO cold call-graph build completed: root={} trigger={trigger} wall_ms=",
        root.display()
    );
    let fields = completed[0]
        .strip_prefix(prefix.as_str())
        .unwrap_or_else(|| panic!("expected prefix {prefix:?}: {completed:?}"));
    let wall_ms: f64 = fields.split_whitespace().next().unwrap().parse().unwrap();
    assert!(wall_ms.is_finite() && wall_ms > 0.0, "{completed:?}");
}

#[test]
fn cold_build_logs_one_completion_for_first_build_and_forced_rebuild() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("lib.rs"), "pub fn callee() {}\n").unwrap();
    let layer = Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"cold build completion"));
    let checkout = Checkout::bind(layer, store.path(), family, root.path()).unwrap();

    for trigger in ["first_build", "forced_rebuild"] {
        let (report, log) = crate::logging::capture_log_lines(|| checkout.build().unwrap());
        assert!(!report.superseded);
        assert_cold_build_log(&log, root.path(), trigger);
    }
}

#[test]
fn refresh_memory_guard_publishes_the_cold_graph_byte_for_byte() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let independent = tempfile::tempdir().unwrap();
    let layer = || Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"refresh memory guard"));
    std::fs::write(root.path().join("go.mod"), "module example.com/p\n").unwrap();
    std::fs::write(
        root.path().join("p.go"),
        "package p\ntype T struct{}\nfunc Use(t T) { t.Run() }\n",
    )
    .unwrap();
    let checkout = Checkout::bind(layer(), store.path(), family, root.path()).unwrap();
    checkout.build().unwrap();
    let before = published_graph_sections(&checkout);

    std::fs::write(root.path().join("run.go"), "package p\nfunc (T) Run() {}\n").unwrap();
    checkout.record_paths(&[b"run.go".to_vec()]).unwrap();
    // Let the update start, then exceed the simulated ceiling during its first file.
    let guard = RefreshMemory::simulated(1, |files, _| Some(if files > 0 { 2 } else { 0 }));
    let (stats, log) = crate::logging::capture_log_lines(|| {
        checkout
            .refresh_callgraph_with(false, &guard)
            .unwrap()
            .unwrap()
    });
    assert!(stats.cold_build);
    assert_cold_build_log(&log, root.path(), "refresh_fallback");
    let work = log
        .iter()
        .filter(|line| line.contains("guard_fallback="))
        .collect::<Vec<_>>();
    assert_eq!(work.len(), 1, "{log:?}");
    assert!(
        work[0].contains("INFO call-graph refresh exceeded its memory ceiling: growth=2 ceiling=1"),
        "{work:?}"
    );
    assert!(
        work[0].contains("guard_fallback=true changed=1")
            && work[0].contains("peak_growth_bytes=2"),
        "{work:?}"
    );
    assert!(checkout.engine.lock().unwrap().is_none());
    assert!(
        checkout.publish_changes().unwrap().is_none(),
        "the cold path already published"
    );
    assert_eq!(
        checkout.refresh_counts(),
        RefreshCounts {
            guard_fallbacks: 1,
            ..Default::default()
        }
    );

    let cold = Checkout::bind(layer(), independent.path(), family, root.path()).unwrap();
    cold.build().unwrap();
    let after = published_graph_sections(&checkout);
    assert!(before != after, "the edit must change the published graph");
    assert!(
        after == published_graph_sections(&cold),
        "cold fallback changed graph section bytes"
    );
}
