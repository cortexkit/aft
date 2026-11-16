pub use aft_parse::callgraph_store::join::dispatch::projected::*;

#[cfg(test)]
mod scratch_tests {
    use super::*;

    fn hint(name: &str) -> MethodHint {
        MethodHint {
            owner: "Owner".to_string(),
            trait_name: Some(format!("Trait\u{0}{name}")),
            name: name.to_string(),
            symbol: format!("Owner::{name}"),
            has_body: true,
            shape: "(&self)".to_string(),
            private: false,
        }
    }

    /// A record with every part set, including empty and missing text, a
    /// zero byte and text that is not ASCII.
    fn record() -> FileRecord {
        let facts = declarations_of(
            "python".to_string(),
            Some("pkg".to_string()),
            vec![
                TypeHint {
                    name: "Owner".to_string(),
                    bases: vec!["Base".to_string(), String::new()],
                    interface: false,
                    closed: true,
                },
                TypeHint {
                    name: "Ünïcode".to_string(),
                    bases: Vec::new(),
                    interface: true,
                    closed: false,
                },
            ],
            vec![hint("run"), hint("")],
        );
        let projections = BTreeMap::from([
            (
                "Base".to_string(),
                ProjectedType {
                    found: Some(("pkg/base.py".to_string(), 3)),
                    dependencies: vec!["r:pkg/base.py".to_string(), "t:x\ty".to_string()]
                        .into_boxed_slice(),
                },
            ),
            (
                "@field:x".to_string(),
                ProjectedType {
                    found: None,
                    dependencies: Box::default(),
                },
            ),
        ]);
        let mro = BTreeMap::from([
            (
                "Owner".to_string(),
                ProjectedMro {
                    mro: Some(vec![
                        ("pkg/a.py".to_string(), "Owner".to_string()),
                        ("pkg/base.py".to_string(), "Base".to_string()),
                    ]),
                    dependencies: vec!["m:run".to_string()].into_boxed_slice(),
                },
            ),
            (
                "Other".to_string(),
                ProjectedMro {
                    mro: None,
                    dependencies: Box::default(),
                },
            ),
        ]);
        let sites = vec![
            SiteRecord {
                site: SiteHint {
                    ordinal: 7,
                    caller: Some("Owner::run".to_string()),
                    line: 12,
                    member: Some("run".to_string()),
                    receiver: Some("@super:Owner".to_string()),
                    dynamic: false,
                },
                associated: true,
                keeps_ordinary_edge: false,
            },
            SiteRecord {
                site: SiteHint {
                    ordinal: u32::MAX,
                    caller: None,
                    line: 0,
                    member: None,
                    receiver: None,
                    dynamic: true,
                },
                associated: false,
                keeps_ordinary_edge: true,
            },
        ];
        FileRecord {
            facts,
            projections,
            mro,
            complete_names: true,
            resolvable_name_projections: 5,
            sites,
            associated: vec![7],
        }
    }

    #[test]
    fn a_scratch_encoded_record_decodes_equal() {
        let record = record();
        let mut bytes = Vec::new();
        record.encode_scratch(&mut bytes).unwrap();
        assert_eq!(FileRecord::decode_scratch(&bytes).unwrap(), record);

        // An empty record too.
        let empty = FileRecord {
            facts: declarations_of(String::new(), None, Vec::new(), Vec::new()),
            projections: BTreeMap::new(),
            mro: BTreeMap::new(),
            complete_names: false,
            resolvable_name_projections: 0,
            sites: Vec::new(),
            associated: Vec::new(),
        };
        let mut empty_bytes = Vec::new();
        empty.encode_scratch(&mut empty_bytes).unwrap();
        assert_eq!(FileRecord::decode_scratch(&empty_bytes).unwrap(), empty);
    }

    #[test]
    fn bytes_that_are_not_one_scratch_record_fail_to_decode() {
        let mut bytes = Vec::new();
        record().encode_scratch(&mut bytes).unwrap();
        for len in 0..bytes.len() {
            assert!(
                FileRecord::decode_scratch(&bytes[..len]).is_err(),
                "a record cut to {len} of {} bytes decoded",
                bytes.len()
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(FileRecord::decode_scratch(&longer).is_err());
        let mut other = bytes;
        other[0] = SCRATCH_FORMAT + 1;
        assert!(FileRecord::decode_scratch(&other).is_err());
    }
}
