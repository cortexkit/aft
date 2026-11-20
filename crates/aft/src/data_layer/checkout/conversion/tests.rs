use super::super::*;
use super::convert_recorded;
use aft_store::family::BuildScratch;

const SOURCE: &str = "class Broken { static void M() {} void Call() { Broken.M(); } }";
const PATH: &str = "Broken.cs";

fn invalid_record() -> ContentRecord {
    let mut record = super::content::extract(SOURCE.as_bytes(), "csharp").unwrap();
    let RecordBody::Parsed { body, .. } = &mut record.data else {
        panic!("expected a parsed C# record");
    };
    // The static dispatch decoder requires a header, even for empty facts.
    assert!(body.static_dispatch.replace(Vec::new()).is_some());
    record
}

fn member(path: &str, bytes: &[u8]) -> PathRecord {
    PathRecord {
        path: path.as_bytes().to_vec(),
        mode: 0o100_644,
        kind: PathKind::Text,
        content: ContentKey::of(bytes),
        link: Vec::new(),
        stat: Default::default(),
        language: "csharp".into(),
    }
}

fn check_warning(logs: &[String], path: &str, reason: &str) {
    let expected = format!("WARN call-graph extraction failed for {path}: {reason}");
    let warnings: Vec<_> = logs
        .iter()
        .filter(|line| line.contains("WARN call-graph extraction failed for"))
        .collect();
    assert_eq!(warnings.len(), 1, "{logs:?}");
    assert!(warnings[0].contains(&expected), "{logs:?}");
}

#[test]
fn record_conversion_failure_is_counted_and_warned_in_cold_build_inputs() {
    for staged in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let family = FamilyId::from_digest(Digest128::of(b"invalid record"));
        let scratch = Arc::new(BuildScratch::create_unattached(root.path(), |_| Ok(())).unwrap());
        let mut sink = BuildSink::new(TextBuild::new(Arc::clone(&scratch), family));
        if staged {
            sink.stage = Some((
                staged_records::RecordStage::new(
                    scratch.subdir("records").unwrap(),
                    family,
                    [1; 32],
                    Default::default(),
                ),
                Arc::new(BlockCache::new(1 << 20)),
            ));
        }
        let path = member(PATH, SOURCE.as_bytes());
        let key = super::content::record_key(path.content, &path.language);
        let invalid = invalid_record();
        let reason = parse_record(&invalid).unwrap_err();
        sink.stored.pending.insert(
            key.digest().to_string(),
            Arc::new(EncodedRecord::of(&invalid)),
        );
        sink.read(
            &path.path,
            path.content,
            ContentKind::Text,
            SOURCE.as_bytes(),
        );
        let good = "class Good {}";
        let good_path = member("Good.cs", good.as_bytes());
        sink.read(
            &good_path.path,
            good_path.content,
            ContentKind::Text,
            good.as_bytes(),
        );
        let state =
            CheckoutState::new(Vec::new(), Digest128::of(b""), vec![path, good_path]).unwrap();
        let mut collected = sink.finish().unwrap();
        let mut report = BuildReport::default();
        let ((inputs, _content), logs) =
            crate::logging::capture_log_lines(|| collected.take_inputs(&state, &mut report));
        assert_eq!(report.extract_failures, 1, "staged={staged}");
        assert_eq!(inputs.parses.len(), 1);
        assert!(inputs.parses.key("Good.cs").is_some());
        assert!(inputs.parses.key(PATH).is_none());
        check_warning(&logs, PATH, &reason);
    }
}

#[test]
fn record_conversion_failure_is_counted_and_warned_in_refresh() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let path = root.path().join(PATH);
    std::fs::write(&path, "class Broken {}\n").unwrap();
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(8 << 20, 2)),
        store.path(),
        FamilyId::from_digest(Digest128::of(b"invalid refresh record")),
        root.path(),
    )
    .unwrap();
    checkout.build().unwrap();
    assert_eq!(checkout.view().unwrap().status().unparsed, 0);
    std::fs::write(&path, SOURCE).unwrap();
    checkout.record_paths(&[PATH.as_bytes().to_vec()]).unwrap();
    let invalid = invalid_record();
    let reason = parse_record(&invalid).unwrap_err();
    let (_, logs) = crate::logging::capture_log_lines(|| {
        // Feed the failed conversion into the extraction slot for this edit.
        let live = checkout.write_live();
        let change = &live.changes[PATH.as_bytes()];
        let mut source = change.content.source.lock().unwrap();
        let failed = convert_recorded(&source.take().unwrap(), &invalid);
        assert!(matches!(failed, Extracted::ConversionFailed));
        assert!(change.content.extracted.set(failed).is_ok());
        drop(source);
        drop(live);
        checkout.refresh_callgraph().unwrap().unwrap();
        assert_eq!(checkout.view().unwrap().status().unparsed, 1);
        assert!(checkout.refresh_callgraph().unwrap().is_none());
    });
    check_warning(&logs, PATH, &reason);
}
