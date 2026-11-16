//! Byte-level comparison of the one no-Luau configuration exception.

use super::*;

#[test]
fn luau_s4_no_luau_luaurc_parity() {
    let temporary = tempfile::tempdir().unwrap();
    let shared = std::env::var("AFT_S4_NO_LUAU_PARITY").ok().map(|label| {
        assert!(label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
        crate::test_support::manifest_dir()
            .join("../..")
            .join(label)
    });
    let directory = shared.as_deref().unwrap_or(temporary.path());
    let root = directory.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let config_bytes = b"{\"aliases\":{\"Unused\":\"absent\"}}\n";
    for (name, bytes) in [
        (".luaurc", config_bytes.as_slice()),
        ("plain.ts", b"export function target() { return 1; }\nexport function caller() { return target(); }\n".as_slice()),
    ] {
        let path = root.join(name);
        if !path.exists() { std::fs::write(&path, bytes).unwrap(); }
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    let mode = if cfg!(feature = "wasm-grammars") {
        "on"
    } else {
        "off"
    };
    let checkout = Checkout::bind(
        Arc::new(DataLayer::new(8 << 20, 1)),
        &directory.join(format!("store-{mode}")),
        FamilyId::from_digest(Digest128::of(b"no-luau-parity")),
        &root,
    )
    .unwrap();
    checkout.build().unwrap();
    let view = checkout.view().unwrap();
    let state = view.current_state();
    assert!(state.skipped_parses().is_empty());
    assert_eq!(
        state.get(b".luaurc").unwrap().language,
        if cfg!(feature = "wasm-grammars") {
            "config"
        } else {
            ""
        }
    );
    assert_eq!(state.get(b"plain.ts").unwrap().language, "typescript");
    let mut other_records = BTreeMap::new();
    let mut luaurc_record = None;
    let mut record_count = 0;
    let mut text = BTreeMap::new();
    let mut graph = BTreeMap::new();
    for segment in &view.published.segments {
        for (handle, info) in segment.sections() {
            let target = if aft_store::text::KNOWN
                .iter()
                .any(|known| known.kind == info.id.kind)
            {
                &mut text
            } else if aft_store::graph::KNOWN
                .iter()
                .any(|known| known.kind == info.id.kind)
            {
                &mut graph
            } else {
                continue;
            };
            let bytes = segment.read_range(handle, 0..info.len).unwrap();
            target
                .entry(format!("{}", info.id))
                .or_insert_with(Vec::new)
                .push(blake3::hash(&bytes).to_hex().to_string());
        }
        let Some(content) = ContentSegment::open(segment).unwrap() else {
            continue;
        };
        record_count += content.len() as usize;
        for record in state
            .paths()
            .iter()
            .filter(|record| !record.language.is_empty())
        {
            let key = crate::data_layer::content::record_key(record.content, &record.language);
            let Some(ordinal) = content.find(key).unwrap() else {
                continue;
            };
            let stored = content.record(ordinal).unwrap();
            let (surface, body) = stored.encode();
            let summary = serde_json::json!({"key": key.digest().to_string(),
                "surface": blake3::hash(&surface).to_hex().to_string(),
                "body": blake3::hash(&body).to_hex().to_string()});
            if record.path == b".luaurc" {
                assert_eq!(stored.language, "config");
                assert_eq!(
                    stored.data,
                    RecordBody::Config {
                        source: config_bytes.to_vec()
                    }
                );
                luaurc_record = Some(summary);
            } else {
                other_records.insert(String::from_utf8(record.path.clone()).unwrap(), summary);
            }
        }
    }
    assert_eq!(
        record_count,
        1 + usize::from(cfg!(feature = "wasm-grammars"))
    );
    assert_eq!(luaurc_record.is_some(), cfg!(feature = "wasm-grammars"));
    assert_eq!(other_records.len(), 1);
    assert!(!text.is_empty() && !graph.is_empty());
    for hashes in text.values_mut().chain(graph.values_mut()) {
        hashes.sort();
    }
    // Observation time is freshness evidence, not content: the two feature
    // builds observe the same untouched files at different moments.
    let paths = state
        .paths()
        .iter()
        .filter(|record| record.path != b".luaurc")
        .map(|record| {
            let mut record = record.clone();
            record.stat.observed_ns = 0;
            record
        })
        .collect::<Vec<_>>();
    let fragments = state
        .paths()
        .iter()
        .enumerate()
        .filter(|(_, record)| record.path != b".luaurc")
        .map(|(index, _)| state.fragment(index))
        .collect();
    let normalized = CheckoutState::new(state.git.clone(), state.conversion, paths)
        .unwrap()
        .with_root_of(state)
        .with_fragments_for(fragments, EXTRACTOR_VERSION)
        .unwrap();
    assert!(state.has_fragments_for(EXTRACTOR_VERSION));
    if shared.is_some() {
        let snapshot = serde_json::json!({"luaurc_record": luaurc_record, "other_records": other_records,
            "text_sections": text, "graph_sections": graph,
            "state_without_luaurc": blake3::hash(&normalized.encode()).to_hex().to_string()});
        std::fs::write(
            directory.join(format!("{mode}.json")),
            serde_json::to_vec_pretty(&snapshot).unwrap(),
        )
        .unwrap();
    }
}
