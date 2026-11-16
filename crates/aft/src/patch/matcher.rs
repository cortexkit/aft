pub use crate::line_matcher::*;

#[cfg(test)]
mod reflow_linear {
    use super::*;

    /// A large reflowed hunk, which the old scan took seconds to search, is
    /// still found and applied.
    #[test]
    fn large_reflowed_hunk_still_applies() {
        let lines = (0..6_000)
            .map(|index| format!("    call_{index}(argument_{index});"))
            .collect::<Vec<_>>();
        let pattern = lines[1_000..3_000]
            .chunks(2)
            .map(|pair| {
                pair.iter()
                    .map(|line| line.trim())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>();
        fn refs(lines: &[String]) -> Vec<&str> {
            lines.iter().map(String::as_str).collect()
        }
        let found = seek_sequence_tiered(&refs(&lines), &refs(&pattern), 0, false).unwrap();
        assert_eq!(
            (found.found, found.tier, found.line_count),
            (1_000, MatchTier::Reflow, 2_000)
        );
        let original = lines.join("\n") + "\n";
        let chunk = crate::patch::parser::UpdateFileChunk {
            old_lines: pattern,
            new_lines: vec!["replaced();".to_owned()],
            change_context: None,
            is_end_of_file: false,
        };
        let patched =
            crate::patch::apply::apply_update_chunks(&original, "big.rs", &[chunk]).unwrap();
        assert_eq!(patched.lines().count(), 6_000 - 2_000 + 1);
        assert!(patched.contains("    call_999(argument_999);\nreplaced();\n    call_3000("));
    }
}
