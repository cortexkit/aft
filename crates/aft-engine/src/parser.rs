pub use aft_parse::parser::*;
#[cfg(test)]
#[test]
fn stage2_search_scoped_parse_source_obeys_whole_read_ceiling() {
    crate::bounded_io::with_skip_disclosure(|| {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.rs");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(crate::large_read_counts::WHOLE_READ_CEILING + 1)
            .unwrap();
        assert!(crate::large_read_counts::is_ceiling_error(
            &read_source(&path).unwrap_err()
        ));
        file.set_len(crate::large_read_counts::WHOLE_READ_CEILING - 1)
            .unwrap();
        assert_eq!(
            read_source(&path).unwrap().len() as u64,
            crate::large_read_counts::WHOLE_READ_CEILING - 1
        );
        crate::protocol::Response::success("parse-ceiling", serde_json::json!({}))
    });
}
