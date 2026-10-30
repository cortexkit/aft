use std::collections::HashSet;

use super::*;
use crate::symbols::{Range, SymbolKind};

const FAMILY: &str = "family";

fn symbol(name: &str) -> Symbol {
    Symbol {
        name: name.to_owned(),
        kind: SymbolKind::Function,
        range: Range {
            start_line: 1,
            start_col: 0,
            end_line: 3,
            end_col: 1,
        },
        signature: Some(format!("fn {name}()")),
        scope_chain: Vec::new(),
        exported: true,
        parent: None,
    }
}

fn key(content: &[u8]) -> SymbolKey {
    SymbolKey {
        content: ContentHash::of(content),
        language: "Rust".to_owned(),
        producer: SymbolProducer::current(),
    }
}

#[test]
fn a_payload_decodes_only_for_its_producer_and_language() {
    let producer = SymbolProducer::current();
    let symbols = vec![symbol("alpha"), symbol("beta")];
    let payload = encode_payload(&producer, "Rust", &symbols);
    assert_eq!(
        decode_payload(&payload, &producer, "Rust").unwrap(),
        symbols
    );
    let other = SymbolProducer {
        extraction_schema: producer.extraction_schema + 1,
    };
    assert!(decode_payload(&payload, &other, "Rust")
        .unwrap_err()
        .contains("producer"));
    assert!(decode_payload(&payload, &producer, "Go")
        .unwrap_err()
        .contains("language"));
    assert!(decode_payload(&payload[..payload.len() - 1], &producer, "Rust").is_err());
    // Every input of the key separates keys.
    let base = key(b"fn alpha() {}");
    assert_ne!(base.family_key(), key(b"fn beta() {}").family_key());
    assert_ne!(
        base.family_key(),
        SymbolKey {
            language: "Go".to_owned(),
            ..base.clone()
        }
        .family_key()
    );
    assert_ne!(
        base.family_key(),
        SymbolKey {
            producer: other,
            ..base.clone()
        }
        .family_key()
    );
    assert_eq!(base.family_key().plane(), FamilyPlane::Symbol);
}

fn sweep(storage: &Path, epoch: i64) -> u64 {
    let lock = v3::ReservationLock::acquire(storage).unwrap();
    let mut registry = v3::RegistryWriter::open_existing(storage, FAMILY, &lock).unwrap();
    let mut cursor = v3::PlaneRowCursor::default();
    let swept = registry
        .sweep_plane_rows(epoch, &HashSet::new(), &mut cursor, 1_000, None)
        .unwrap();
    assert!(swept.complete);
    swept.deleted
}

fn has(storage: &Path, plane: FamilyPlane, key: &FamilyKey) -> bool {
    v3::PlaneReader::open_existing(storage, FAMILY, plane)
        .unwrap()
        .contains(key)
        .unwrap()
}

/// No manifest marks a symbol row, so the plane-row sweep ages symbol rows
/// out by epoch: an untouched row survives `SYMBOL_RETENTION_SWEEPS` sweeps,
/// a touch restarts its count, and an unmarked row of a manifest plane is
/// still collected at the first sweep.
#[test]
fn the_sweep_keeps_symbol_rows_for_their_retention_epochs() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let storage = storage.path();
    let producer = SymbolProducer::current();
    let kept = key(b"fn kept() {}").family_key();
    let touched = key(b"fn touched() {}").family_key();
    let rows = [kept, touched]
        .iter()
        .map(|key| (*key, encode_payload(&producer, "Rust", &[symbol("row")])))
        .collect::<Vec<_>>();
    assert_eq!(put(storage, FAMILY, &rows).unwrap(), 2);
    assert_eq!(
        put(storage, FAMILY, &rows).unwrap(),
        0,
        "a second put reuses"
    );
    let trigram = FamilyKey::new(FamilyPlane::Trigram, [3; 32]);
    v3::publish(storage, FAMILY, 1 << 20, None, |publication| {
        publication.put(&trigram, b"unmarked", 1)
    })
    .unwrap();

    sweep(storage, 1);
    assert!(!has(storage, FamilyPlane::Trigram, &trigram));
    for epoch in 2..=SYMBOL_RETENTION_SWEEPS {
        if epoch == 4 {
            assert!(touch(storage, FAMILY, &[touched]).unwrap().is_empty());
        }
        assert_eq!(sweep(storage, epoch), 0, "epoch {epoch}");
    }
    assert!(has(storage, FamilyPlane::Symbol, &kept));
    // The untouched row was stamped at epoch 0 and is now one sweep too old.
    assert_eq!(sweep(storage, SYMBOL_RETENTION_SWEEPS + 1), 1);
    assert!(!has(storage, FamilyPlane::Symbol, &kept));
    assert!(has(storage, FamilyPlane::Symbol, &touched));
    // The touch at epoch 3's stamp keeps the other row until epoch 3 + 8 + 1.
    for epoch in SYMBOL_RETENTION_SWEEPS + 2..=3 + SYMBOL_RETENTION_SWEEPS {
        assert_eq!(sweep(storage, epoch), 0, "epoch {epoch}");
    }
    assert_eq!(sweep(storage, 4 + SYMBOL_RETENTION_SWEEPS), 1);
    assert_eq!(
        v3::v3_format_report(&v3::plane_path(storage, FAMILY, FamilyPlane::Symbol).unwrap())
            .unwrap()
            .layout_tag,
        v3::LAYOUT_TAG
    );
}

#[test]
fn a_lookup_reads_a_stored_row_and_misses_without_parsing() {
    let _serial = v3::serial_for_test();
    let storage = tempfile::tempdir().unwrap();
    let storage = storage.path();
    let source = b"pub fn stored() {}\n";
    let path = Path::new("src/stored.rs");
    assert!(lookup(storage, FAMILY, ContentHash::of(source), path)
        .unwrap()
        .is_none());
    let producer = SymbolProducer::current();
    let language = language_of(path).unwrap();
    let key = SymbolKey {
        content: ContentHash::of(source),
        language: language.clone(),
        producer,
    };
    let symbols = vec![symbol("stored")];
    put(
        storage,
        FAMILY,
        &[(
            key.family_key(),
            encode_payload(&producer, &language, &symbols),
        )],
    )
    .unwrap();
    let found = lookup(storage, FAMILY, ContentHash::of(source), path)
        .unwrap()
        .unwrap();
    assert_eq!(*found, symbols);
    // The same content under another language has another key.
    assert!(lookup(
        storage,
        FAMILY,
        ContentHash::of(source),
        Path::new("src/stored.go")
    )
    .unwrap()
    .is_none());
}
