mod tests {
    use crate::search_index::bounded_grep_line_text_bytes;

    #[test]
    fn semantic_namespaces_share_type_identity() {
        use std::any::TypeId;
        assert_eq!(
            TypeId::of::<crate::search_index::exact_lane::QualifiedMember>(),
            TypeId::of::<crate::commands::semantic_search::exact_lane::QualifiedMember>()
        );
        assert_eq!(
            TypeId::of::<crate::search_index::memo::ExactMemoStore>(),
            TypeId::of::<crate::commands::semantic_search::memo::ExactMemoStore>()
        );
    }

    #[test]
    fn bounded_line_text_renders_exactly_like_the_whole_line() {
        use crate::commands::grep::{truncate_grep_line, truncate_line_text, GREP_MAX_LINE_CHARS};

        let mut invalid = "é".repeat(GREP_MAX_LINE_CHARS - 1).into_bytes();
        invalid.extend_from_slice(&[0xff, 0xe2, 0x82, b'x']);
        invalid.extend_from_slice(&"tail".repeat(2_000).into_bytes());
        let lines: Vec<Vec<u8>> = vec![
            b"short line".to_vec(),
            Vec::new(),
            "a".repeat(GREP_MAX_LINE_CHARS).into_bytes(),
            "a".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "é".repeat(GREP_MAX_LINE_CHARS + 1).into_bytes(),
            "€".repeat(5_000).into_bytes(),
            "needle-pad".repeat(100_000).into_bytes(),
            invalid,
        ];
        for line in &lines {
            let whole = String::from_utf8_lossy(line).into_owned();
            let bounded = bounded_grep_line_text_bytes(line);
            assert_eq!(truncate_grep_line(&bounded), truncate_grep_line(&whole));
            assert_eq!(truncate_line_text(&bounded), truncate_line_text(&whole));
            if whole.chars().count() <= GREP_MAX_LINE_CHARS {
                assert_eq!(bounded, whole, "a line within the bound is kept whole");
            }
            assert!(bounded.chars().count() <= GREP_MAX_LINE_CHARS + 20);
        }
    }
}
