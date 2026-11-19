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
