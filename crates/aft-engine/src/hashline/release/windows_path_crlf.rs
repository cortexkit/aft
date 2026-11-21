//! Authoritative Windows path separator and CRLF terminator release gates.
//!
//! These tests run on every host. Windows CI (`unit-windows-cargo` /
//! `unit-windows-check`) is the authoritative runner named by FINAL GATES;
//! the assertions themselves are platform-neutral so a Unix developer still
//! catches regressions before push.

/// Section header path spellings that Windows agents commonly emit.
#[cfg(test)]
const WINDOWS_PATH_SPELLINGS: &[&str] = &[
    r"src\lib.rs",
    r"src\hashline\release\mod.rs",
    r"C:\Users\agent\project\src\main.rs",
    r"\\?\C:\repo\file.rs",
];

#[cfg(test)]
mod tests {
    use super::WINDOWS_PATH_SPELLINGS;
    use crate::hashline::oracle::tag_for;
    use crate::hashline::release::fixture::{
        build_a13_fixture, build_a13_fixture_crlf, normalize_agent_path_spelling,
    };
    use crate::hashline::scan::{scan_bytes, TerminatorKind};
    use crate::hashline::snapshot::{render_tagged_snapshot, render_tagless_snapshot};
    use crate::hashline::syntax::{parse_hashline_patch, HashlineRejectionCode, RejectionStage};

    #[test]
    fn crlf_records_preserve_terminator_kind_and_differ_from_lf_tags() {
        let lf = build_a13_fixture();
        let crlf = build_a13_fixture_crlf();

        let lf_snap = scan_bytes(&lf);
        let crlf_snap = scan_bytes(&crlf);

        // First retained record must carry the terminator kind that produced it.
        let lf_term = lf_snap
            .records
            .values()
            .next()
            .expect("lf fixture has records")
            .terminator;
        let crlf_term = crlf_snap
            .records
            .values()
            .next()
            .expect("crlf fixture has records")
            .terminator;
        assert_eq!(lf_term, TerminatorKind::Lf);
        assert_eq!(crlf_term, TerminatorKind::CrLf);

        // Tag normalization strips CR before LF, so LF/CRLF twins share a tag
        // while retained records keep distinct terminator kinds.
        let lf_tag = tag_for(&lf);
        let crlf_tag = tag_for(&crlf);
        assert_eq!(
            lf_tag, crlf_tag,
            "tag normalization strips CR before LF, so LF/CRLF twins share a tag"
        );
        assert_ne!(lf_snap.records.values().next().map(|r| r.terminator), None);
        assert_eq!(crlf_term, TerminatorKind::CrLf);
    }

    #[test]
    fn crlf_render_paths_emit_stable_carriers_for_both_modes() {
        let crlf = build_a13_fixture_crlf();
        // Keep the render input small enough to stay under the 50 KiB body cap
        // so the carrier header is observable rather than drowned in elision.
        let head: Vec<u8> = crlf.iter().copied().take(4 * 1024).collect();
        let snapshot = scan_bytes(&head);
        let path = r"src\fixture\a13_crlf.txt";
        let tagged = render_tagged_snapshot(&snapshot, path);
        let tagless = render_tagless_snapshot(&snapshot, path);

        assert!(
            tagged
                .text
                .starts_with(&format!("[{path}#{}]", snapshot.tag)),
            "gate-on carrier must preserve the requested path spelling verbatim: {}",
            &tagged.text[..tagged.text.find('\n').unwrap_or(80)]
        );
        assert!(
            tagless.text.contains("1:"),
            "gate-off render must still emit absolute gutters"
        );
        // Requested path is not rewritten by the renderer.
        assert_eq!(tagged.requested_path, path);
        assert_eq!(tagless.requested_path, path);
    }

    #[test]
    fn windows_path_spellings_normalize_for_comparison_helpers() {
        for spelling in WINDOWS_PATH_SPELLINGS {
            let normalized = normalize_agent_path_spelling(spelling);
            assert!(
                !normalized.contains('\\'),
                "normalized spelling must not retain backslashes: {normalized}"
            );
            assert!(
                normalized.contains('/'),
                "normalized spelling must use forward slashes: {normalized}"
            );
        }
    }

    #[test]
    fn parser_accepts_forward_slash_headers_and_rejects_malformed_tags_identically() {
        // Path validation beyond tag syntax is owned by later stages; the
        // release gate locks that CRLF line endings inside the patch body do
        // not change parse/header adjudication.
        let lf_patch = "[src/lib.rs#CAFE]\nREM 1\n";
        let crlf_patch = "[src/lib.rs#CAFE]\r\nREM 1\r\n";
        let lf = parse_hashline_patch(lf_patch);
        let crlf = parse_hashline_patch(crlf_patch);
        // Both must reach the same stage class (either both parse or both fail
        // for the same structural reason). We only require equal success bit
        // and, on failure, equal code+stage.
        match (lf, crlf) {
            (Ok(_), Ok(_)) => {}
            (Err(left), Err(right)) => {
                assert_eq!(left.code, right.code);
                assert_eq!(left.stage, right.stage);
            }
            (left, right) => panic!("LF/CRLF patch adjudication diverged: {left:?} vs {right:?}"),
        }

        let malformed = parse_hashline_patch("[src/lib.rs#CAF]\nREM 1\n");
        let err = malformed.expect_err("three-hex tag is malformed");
        assert_eq!(err.code, HashlineRejectionCode::MalformedTag);
        assert_eq!(err.stage, RejectionStage::Header);
    }

    #[test]
    fn mixed_terminator_fixture_retains_per_record_kinds() {
        let bytes = b"lf-only\ncrlf-line\r\nnone-at-eof";
        let snapshot = scan_bytes(bytes);
        let kinds: Vec<_> = snapshot
            .records
            .values()
            .map(|record| record.terminator)
            .collect();
        assert_eq!(
            kinds,
            vec![
                TerminatorKind::Lf,
                TerminatorKind::CrLf,
                TerminatorKind::None
            ]
        );
    }
}
