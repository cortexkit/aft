use super::super::*;
use super::{convert_recorded, extract_recorded};
use crate::data_layer::content::{parses_of_for_test, record_key};
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
        let recorded = source.take().unwrap();
        let key = record_key(recorded.content, &recorded.language);
        let failed = convert_recorded(&recorded.path, key, &invalid);
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

fn bound_checkout(root: &Path, store: &Path) -> Checkout {
    Checkout::bind(
        Arc::new(DataLayer::new(8 << 20, 2)),
        store,
        FamilyId::from_digest(Digest128::of(b"changed content reuse")),
        root,
    )
    .unwrap()
}

fn changed_source(checkout: &Checkout, path: &str) -> (Arc<ChangeContent>, Arc<RecordedSource>) {
    let change = Arc::clone(&checkout.live.read().unwrap().changes[path.as_bytes()].content);
    assert!(change.peek().is_none(), "recording does not extract");
    let source = change.source.lock().unwrap().clone().unwrap();
    (change, source)
}

#[test]
fn refresh_reuses_a_sibling_content_record_without_parsing() {
    let sibling_root = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let checkout = bound_checkout(root.path(), store.path());
    std::fs::write(root.path().join("unrelated.rs"), "fn unrelated() {}\n").unwrap();
    checkout.build().unwrap();

    let source =
        b"// TODO: keep the record across paths\nfn original() {}\nfn caller() { original(); }\n";
    let sibling = bound_checkout(sibling_root.path(), store.path());
    std::fs::write(sibling_root.path().join("original.rs"), source).unwrap();
    sibling.build().unwrap();

    let path = "copied.rs";
    std::fs::write(root.path().join(path), source).unwrap();
    checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
    let (change, recorded) = changed_source(&checkout, path);
    let before = parses_of_for_test(source);
    checkout.refresh_callgraph().unwrap().unwrap();
    assert_eq!(parses_of_for_test(source), before, "a hit never parses");

    let Extracted::Parse(key, parse, encoded) = change.get() else {
        panic!("expected a parsed record on a hit");
    };
    let Extracted::Parse(fresh_key, fresh_parse, fresh_encoded) =
        extract_recorded(&recorded, &StoredRecords::default())
    else {
        panic!("expected a freshly parsed record");
    };
    assert_eq!(key, &fresh_key);
    assert_eq!(parse, &fresh_parse);
    assert_eq!(encoded, &fresh_encoded);
    checkout.publish_changes().unwrap().unwrap();
    let published = checkout
        .family_records()
        .unwrap()
        .record(record_key(recorded.content, &recorded.language));
    assert_eq!(
        published.as_ref().map(EncodedRecord::of),
        Some((*fresh_encoded).clone())
    );
}

#[test]
fn refresh_parses_a_content_record_miss() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let checkout = bound_checkout(root.path(), store.path());
    std::fs::write(root.path().join("previous.rs"), "fn previous() {}\n").unwrap();
    checkout.build().unwrap();

    let path = "new.rs";
    let source = b"fn never_stored_for_this_refresh() {}\n";
    std::fs::write(root.path().join(path), source).unwrap();
    checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
    let (change, recorded) = changed_source(&checkout, path);
    assert!(checkout
        .family_records()
        .unwrap()
        .record(record_key(recorded.content, &recorded.language))
        .is_none());
    let before = parses_of_for_test(source);
    checkout.refresh_callgraph().unwrap().unwrap();
    assert_eq!(parses_of_for_test(source), before + 1);
    assert!(matches!(change.get(), Extracted::Parse(..)));
}

#[test]
fn refresh_reuses_config_comment_markers_without_parsing() {
    let root = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let checkout = bound_checkout(root.path(), store.path());
    let source = b"# TODO: carry the workspace marker\n[workspace]\nmembers = []\n";
    std::fs::write(root.path().join("Cargo.toml"), source).unwrap();
    checkout.build().unwrap();

    let path = "nested/Cargo.toml";
    std::fs::create_dir(root.path().join("nested")).unwrap();
    std::fs::rename(root.path().join("Cargo.toml"), root.path().join(path)).unwrap();
    checkout
        .record_paths(&[b"Cargo.toml".to_vec(), path.as_bytes().to_vec()])
        .unwrap();
    let (change, recorded) = changed_source(&checkout, path);
    let before = parses_of_for_test(source);
    let markers = checkout
        .todo_markers(
            &checkout.view().unwrap(),
            &[path.as_bytes().to_vec()],
            u64::MAX,
        )
        .unwrap();
    assert!(matches!(markers.as_slice(), [PathTodos::Recorded(Some(_))]));
    checkout.refresh_callgraph().unwrap().unwrap();
    assert_eq!(
        parses_of_for_test(source),
        before,
        "stored config TODOs need no grammar"
    );

    let Extracted::Config(config, encoded) = change.get() else {
        panic!("expected a config record");
    };
    let Extracted::Config(fresh_config, fresh_encoded) =
        extract_recorded(&recorded, &StoredRecords::default())
    else {
        panic!("expected a freshly extracted config record");
    };
    assert!(fresh_encoded.todos().unwrap().is_some());
    assert_eq!(config, &fresh_config);
    assert_eq!(encoded, &fresh_encoded);
}
