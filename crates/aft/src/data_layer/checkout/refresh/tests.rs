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
fn refresh_reader_merge_releases_decoded_files_and_publishes_cold_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let layer = || Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"refresh reader merge"));
    std::fs::write(root.join("go.mod"), "module example.com/p\n").unwrap();
    std::fs::write(root.join("p.go"), "package p\ntype T struct{}\n").unwrap();
    let readers = 67;
    for i in 0..readers {
        std::fs::write(
            root.join(format!("reader{i}.go")),
            format!("package p\nfunc Use{i}(t T) {{ t.Run() }}\n"),
        )
        .unwrap();
    }

    let checkout = Checkout::bind(layer(), &dir.path().join("live"), family, &root).unwrap();
    checkout.build().unwrap();
    let reader = |checkout: &Checkout| {
        checkout
            .inspect_engine(|live| live.resolved.read("reader0.go").unwrap())
            .unwrap()
            .unwrap()
    };
    let before = reader(&checkout);
    assert!(!before.sites.is_empty());

    std::fs::write(root.join("run.go"), "package p\nfunc (T) Run() {}\n").unwrap();
    checkout.record_paths(&[b"run.go".to_vec()]).unwrap();
    let stats = checkout.refresh_callgraph().unwrap().unwrap();
    assert!(
        !stats.cold_build,
        "the fixture needs a real incremental merge"
    );
    assert_eq!(stats.sites_resolved, readers + 1);
    // Check immediately after assembly, before the engine's general cache trim.
    assert_eq!(
        super::super::source_work_for_test(&root).decoded_resolutions,
        0
    );
    assert_ne!(before.sites, reader(&checkout).sites);
    assert!(checkout.publish_changes().unwrap().is_some());
    assert!(matches!(checkout.fold().unwrap(), Folded::Done(_)));

    let cold = Checkout::bind(layer(), &dir.path().join("cold"), family, &root).unwrap();
    cold.build().unwrap();
    assert!(published_graph_sections(&checkout) == published_graph_sections(&cold));
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

#[test]
fn refresh_cache_churn_falls_back_to_byte_identical_cold_publication() {
    use aft_store::heap::heap_bytes_of;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let layer = || Arc::new(DataLayer::new(8 << 20, 2));
    let family = FamilyId::from_digest(Digest128::of(b"refresh declarer churn"));
    std::fs::write(root.join("go.mod"), "module example.com/p\n").unwrap();
    std::fs::write(root.join("shape.go"), "package p\ntype Shape struct{}\n").unwrap();
    for (path, ty) in [("a.go", "A"), ("b.go", "B")] {
        let mut source = format!("package p\ntype {ty} struct{{}}\n");
        for n in 0..40 {
            source.push_str(&format!("func ({ty}) Hit{n}() {{}}\n"));
        }
        std::fs::write(root.join(path), source).unwrap();
    }
    // Every reader depends on Run. Each member name needs a fresh answer,
    // alternating between declarations that cannot both fit in the cache.
    for n in 0..72 {
        let mut source = format!("package p\nfunc Read{n}(s Shape, a A, b B) {{\ns.Run()\n");
        for member in 0..40 {
            source.push_str(&format!("a.Hit{member}()\nb.Hit{member}()\n"));
        }
        source.push_str("}\n");
        std::fs::write(root.join(format!("read{n}.go")), source).unwrap();
    }

    let checkout = Checkout::bind(layer(), &dir.path().join("live"), family, &root).unwrap();
    checkout.build().unwrap();
    let before = published_graph_sections(&checkout);
    let budget = checkout
        .inspect_engine(|live| {
            let calls = live.call_inputs_for_test();
            ["a.go", "b.go"]
                .into_iter()
                .map(|path| {
                    assert_eq!(
                        calls
                            .projected_methods_for_test(&live.inputs, &live.resolved, path)
                            .unwrap()
                            .len(),
                        40
                    );
                    heap_bytes_of(&calls.cached_for_test(path).unwrap()) + path.len()
                })
                .max()
                .unwrap()
        })
        .unwrap()
        .unwrap();
    {
        let published = checkout.view().unwrap().published;
        let mut slot = checkout.engine.lock().unwrap();
        checkout.ensure_engine(&mut slot, &published).unwrap();
        slot.as_mut()
            .unwrap()
            .live
            .set_projection_budget_for_test(budget);
    }
    std::fs::write(root.join("run.go"), "package p\nfunc (Shape) Run() {}\n").unwrap();
    checkout.record_paths(&[b"run.go".to_vec()]).unwrap();
    let guard = RefreshMemory::simulated(u64::MAX, |_, _| Some(0));
    let (stats, log) = crate::logging::capture_log_lines(|| {
        checkout
            .refresh_callgraph_with(false, &guard)
            .unwrap()
            .unwrap()
    });
    assert!(stats.cold_build, "{stats:?}");
    assert_eq!(checkout.refresh_counts().guard_fallbacks, 1);
    assert_eq!(checkout.refresh_counts().precheck_fallbacks, 0);
    let reasons: Vec<_> = log
        .iter()
        .filter(|line| line.contains("guard_fallback=true"))
        .collect();
    assert_eq!(reasons.len(), 1, "{log:?}");
    assert!(
        reasons[0].contains("reason=cache_churn") && reasons[0].contains("redecodes=4096"),
        "{reasons:?}"
    );
    assert!(
        reasons[0].contains(&format!("budget={budget};")),
        "{reasons:?}"
    );
    assert!(checkout.engine.lock().unwrap().is_none());
    assert!(checkout.publish_changes().unwrap().is_none());

    let cold = Checkout::bind(layer(), &dir.path().join("cold"), family, &root).unwrap();
    cold.build().unwrap();
    let after = published_graph_sections(&checkout);
    assert!(
        after != before,
        "the edited graph must differ from the old publication"
    );
    assert!(
        after == published_graph_sections(&cold),
        "cache churn fallback changed graph bytes"
    );
}
