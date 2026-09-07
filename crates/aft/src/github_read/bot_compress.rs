//! Fixture-derived render compression for GitHub discussion bodies.
//!
//! The raw PR 270 and PR 283 payloads pinned beside this module establish the
//! machine formats rather than merely supplying sample prose. Cubic review
//! bodies put summary banners between `<!-- cubic:review-summary:* -->`
//! markers. Cubic findings are separate review-thread comments: each comment
//! has its own `<!-- cubic:v=... -->` marker, while the finding itself starts
//! `P1:`/`P2:`/`P3:` and the file location appears later inside a `<details>`
//! block. Greptile's summary and detail blocks are bounded by
//! `<!-- greptile_comment -->` markers, while its inline priority badge is an
//! image served from `greptile-static-assets`. Codesmith appends
//! `<!-- codesmith:footer -->` tracking blocks even to bodies written by human
//! authors. Every extractor below is constrained by at least one of those
//! observed fixture shapes.

use std::sync::LazyLock;

use regex::Regex;

static CODESMITH_FOOTER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<!--\s*codesmith:footer\s*-->.*?<!--\s*/codesmith:footer\s*-->")
        .expect("valid codesmith footer regex")
});
static CUBIC_MARKER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<!--\s*cubic:").expect("valid cubic marker regex"));
static CUBIC_SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<!--\s*cubic:review-summary:start\s*-->(.*?)<!--\s*cubic:review-summary:end\s*-->",
    )
    .expect("valid cubic summary regex")
});
static CUBIC_FINDING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(P[0-3]:[^\r\n]+)").expect("valid cubic finding regex"));
static CUBIC_LOCATION_AT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)At ([^,\r\n]+), line ([0-9]+):").expect("valid cubic details location regex")
});
static CUBIC_LOCATION_ATTRIBUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"location="([^"\r\n]+:[0-9]+)""#).expect("valid cubic location attribute regex")
});
static GREPTILE_SECTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<!--\s*greptile_comment\s*-->(.*?)<!--\s*/greptile_comment\s*-->")
        .expect("valid greptile section regex")
});
static GREPTILE_BADGE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<a\b[^>]*>\s*<img\b[^>]*\balt="(P[0-3])"[^>]*greptile-static-assets[^>]*>\s*</a>\s*(?:\*\*([^*\r\n]+)\*\*)?"#,
    )
    .expect("valid greptile priority badge regex")
});
static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("valid HTML comment regex"));
static DETAILS_BODY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<details\b[^>]*>.*?</details>").expect("valid details regex")
});
static PICTURE_ANCHOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<a\b[^>]*>\s*<picture\b[^>]*>.*?</picture>\s*</a>")
        .expect("valid picture anchor regex")
});
static PICTURE_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<picture\b[^>]*>.*?</picture>").expect("valid picture regex")
});
static IMAGE_ANCHOR_GROUP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^[ \t]*(?:<a\b[^>]*>[ \t]*<img\b[^>]*>[ \t]*</a>[ \t]*)+$")
        .expect("valid image-anchor group regex")
});
static CUBIC_TRAILER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<sub>.*?(?:Re-trigger cubic|cubic CLI).*?</sub>")
        .expect("valid cubic trailer regex")
});
static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").expect("valid HTML tag regex"));

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompressedBody {
    pub(super) body: String,
    pub(super) compressed: bool,
}

/// Remove only machine comments and tracking/button structures. Text and
/// ordinary HTML details remain available to full discussion drill-downs.
pub(super) fn structural_strip(body: &str) -> String {
    let without_codesmith = CODESMITH_FOOTER_RE.replace_all(body, "");
    let without_comments = HTML_COMMENT_RE.replace_all(&without_codesmith, "");
    let without_cubic_trailers = CUBIC_TRAILER_RE.replace_all(&without_comments, "");
    let without_image_groups = IMAGE_ANCHOR_GROUP_RE.replace_all(&without_cubic_trailers, "");
    let without_picture_links = PICTURE_ANCHOR_RE.replace_all(&without_image_groups, "");
    let without_pictures = PICTURE_BLOCK_RE.replace_all(&without_picture_links, "");
    trim_removed_edges(without_pictures.as_ref()).to_string()
}

/// Compress marked machine sections appended to the pull-request or issue body
/// without treating its human author as a bot.
pub(super) fn compress_document_body(body: &str) -> CompressedBody {
    let without_codesmith = CODESMITH_FOOTER_RE.replace_all(body, "");
    let had_codesmith = without_codesmith.len() != body.len();
    let mut had_greptile = false;
    let with_greptile =
        GREPTILE_SECTION_RE.replace_all(&without_codesmith, |captures: &regex::Captures<'_>| {
            had_greptile = true;
            compress_greptile(captures.get(1).map_or("", |capture| capture.as_str()))
        });
    CompressedBody {
        body: structural_strip(with_greptile.as_ref()),
        compressed: had_codesmith || had_greptile,
    }
}

pub(super) fn compress_discussion_body(author: Option<&str>, body: &str) -> CompressedBody {
    if body.is_empty() {
        return CompressedBody {
            body: String::new(),
            compressed: false,
        };
    }
    let without_codesmith = CODESMITH_FOOTER_RE.replace_all(body, "");
    if trim_removed_edges(&without_codesmith).is_empty() {
        return CompressedBody {
            body: String::new(),
            compressed: false,
        };
    }
    if CUBIC_MARKER_RE.is_match(&without_codesmith) {
        return CompressedBody {
            body: compress_cubic(&without_codesmith),
            compressed: true,
        };
    }
    if GREPTILE_SECTION_RE.is_match(&without_codesmith)
        || GREPTILE_BADGE_RE.is_match(&without_codesmith)
    {
        return CompressedBody {
            body: compress_greptile(&without_codesmith),
            compressed: true,
        };
    }
    if author.is_some_and(is_bot_login) {
        return CompressedBody {
            body: compress_generic_bot(&without_codesmith),
            compressed: true,
        };
    }
    CompressedBody {
        body: structural_strip(&without_codesmith),
        compressed: false,
    }
}

fn compress_cubic(body: &str) -> String {
    let label = if CUBIC_SUMMARY_RE.is_match(body) {
        "[cubic review, compressed]"
    } else {
        "[cubic comment, compressed]"
    };
    let mut lines = vec![label.to_string()];

    for captures in CUBIC_SUMMARY_RE.captures_iter(body) {
        let summary = captures
            .get(1)
            .map(|capture| strip_html_tags(capture.as_str()))
            .unwrap_or_default();
        for line in summary
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
        {
            push_unique(&mut lines, line.to_string());
        }
    }

    let location = cubic_location(body);
    for captures in CUBIC_FINDING_RE.captures_iter(body) {
        let finding = captures.get(1).expect("finding capture exists").as_str();
        let mut finding = first_sentence(finding).to_string();
        if let Some(location) = location.as_deref() {
            finding.push_str(" (");
            finding.push_str(location);
            finding.push(')');
        }
        push_unique(&mut lines, finding);
    }

    for line in body.lines().map(str::trim) {
        if line.starts_with("✅ Addressed in ") {
            push_unique(&mut lines, line.to_string());
        }
    }
    lines.join("\n")
}

fn cubic_location(body: &str) -> Option<String> {
    if let Some(captures) = CUBIC_LOCATION_AT_RE.captures(body) {
        return Some(format!(
            "{}:{}",
            captures.get(1)?.as_str(),
            captures.get(2)?.as_str()
        ));
    }
    CUBIC_LOCATION_ATTRIBUTE_RE
        .captures(body)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_string())
}

fn compress_greptile(body: &str) -> String {
    let mut lines = vec!["[greptile comment, compressed]".to_string()];

    for captures in GREPTILE_BADGE_RE.captures_iter(body) {
        let priority = captures.get(1).expect("priority capture exists").as_str();
        let title = captures
            .get(2)
            .map(|capture| collapse_whitespace(capture.as_str()))
            .unwrap_or_default();
        let detail = captures
            .get(0)
            .and_then(|badge| first_prose_paragraph(&body[badge.end()..]));
        let finding = match (title.is_empty(), detail) {
            (false, Some(detail)) => {
                format!("{priority}: {title} — {}", first_sentence(detail.trim()))
            }
            (false, None) => format!("{priority}: {title}"),
            (true, Some(detail)) => format!("{priority}: {}", first_sentence(detail.trim())),
            (true, None) => continue,
        };
        push_unique(&mut lines, finding);
    }

    for line in body.lines().map(str::trim) {
        if matches!(line.as_bytes(), [b'P', b'0'..=b'3', b':', ..]) {
            push_unique(&mut lines, first_sentence(line).to_string());
        } else if line.starts_with("- ")
            && !line.contains("Knowledge Base")
            && !line.contains("app.greptile.com")
            && !line.contains("custom-context")
        {
            let clean = strip_html_tags(line);
            if !clean.is_empty() {
                push_unique(&mut lines, clean);
            }
        }
    }
    lines.join("\n")
}

fn compress_generic_bot(body: &str) -> String {
    let without_comments = HTML_COMMENT_RE.replace_all(body, "");
    let without_details = DETAILS_BODY_RE.replace_all(&without_comments, "");
    let without_tags = strip_html_tags(&without_details);
    let clean = trim_removed_edges(&without_tags);
    let label = "[bot comment, compressed]";
    let Some(paragraph) = clean
        .split("\n\n")
        .map(str::trim)
        .find(|paragraph| !paragraph.is_empty())
    else {
        return label.to_string();
    };
    let raw_lines = body.lines().count();
    let kept_lines = paragraph.lines().count();
    format!(
        "{label}\n{paragraph}\n[compressed: {} lines dropped]",
        raw_lines.saturating_sub(kept_lines)
    )
}

fn first_prose_paragraph(body: &str) -> Option<String> {
    let without_details = DETAILS_BODY_RE.replace_all(body, "");
    let without_comments = HTML_COMMENT_RE.replace_all(&without_details, "");
    let without_tags = strip_html_tags(&without_comments);
    without_tags
        .split("\n\n")
        .map(str::trim)
        .find(|paragraph| {
            !paragraph.is_empty()
                && !paragraph.starts_with("**Knowledge Base Used:**")
                && !paragraph.starts_with("Knowledge Base Used:")
        })
        .map(collapse_whitespace)
}

fn first_sentence(line: &str) -> &str {
    let mut in_code = false;
    for (index, character) in line.char_indices() {
        if character == '`' {
            in_code = !in_code;
            continue;
        }
        if !in_code && matches!(character, '.' | '!' | '?') {
            let end = index + character.len_utf8();
            if line[end..].chars().next().is_none_or(char::is_whitespace) {
                return &line[..end];
            }
        }
    }
    line
}

fn is_bot_login(author: &str) -> bool {
    let normalized = author.trim_start_matches('@').to_ascii_lowercase();
    normalized.ends_with("[bot]")
        || matches!(
            normalized.as_str(),
            "cubic-dev-ai"
                | "greptile-apps"
                | "codesmith-bot"
                | "dependabot"
                | "github-actions"
                | "github-actions[bot]"
        )
}

fn strip_html_tags(value: &str) -> String {
    HTML_TAG_RE.replace_all(value, "").trim().to_string()
}

fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn trim_removed_edges(value: &str) -> &str {
    value.trim_matches(|character: char| character.is_whitespace())
}

fn push_unique(lines: &mut Vec<String>, line: String) {
    if !lines.iter().any(|existing| existing == &line) {
        lines.push(line);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::{compress_discussion_body, structural_strip};
    use crate::github_read::normalize::normalize_structured_document;
    use crate::github_read::render::render_document_for_resource;
    use crate::github_read::resource::parse_resource;
    use crate::github_read::{GithubDocument, GithubReadError};

    const PR_270_JSON: &str = include_str!("fixtures/pr-270.json");
    const PR_270_REVIEWS_JSON: &str = include_str!("fixtures/pr-270-review-comments.json");
    const PR_283_JSON: &str = include_str!("fixtures/pr-283.json");
    const PR_283_REVIEWS_JSON: &str = include_str!("fixtures/pr-283-review-comments.json");

    fn fixture(number: u64) -> GithubDocument {
        let resource = parse_resource(&format!("pr://cortexkit/aft/{number}"))
            .expect("fixture resource is valid");
        let (main, reviews) = match number {
            270 => (PR_270_JSON, PR_270_REVIEWS_JSON),
            283 => (PR_283_JSON, PR_283_REVIEWS_JSON),
            _ => panic!("no fixture for PR {number}"),
        };
        let mut document = normalize_structured_document(
            &resource,
            &serde_json::from_str::<Value>(main).expect("main fixture is JSON"),
        )
        .expect("main fixture normalizes");
        let review_document = normalize_structured_document(
            &resource,
            &serde_json::from_str::<Value>(reviews).expect("review fixture is JSON"),
        )
        .expect("review fixture normalizes");
        document.review_comment_sections = review_document.review_comment_sections;
        document
    }

    fn fixture_first_sentence(line: &str) -> &str {
        let mut in_code = false;
        for (index, character) in line.char_indices() {
            if character == '`' {
                in_code = !in_code;
                continue;
            }
            if !in_code && matches!(character, '.' | '!' | '?') {
                let end = index + character.len_utf8();
                if line[end..].chars().next().is_none_or(char::is_whitespace) {
                    return &line[..end];
                }
            }
        }
        line
    }

    fn fixture_cubic_location(body: &str) -> Option<String> {
        body.split_once(". At ")
            .and_then(|(_, tail)| tail.split_once(", line "))
            .and_then(|(path, tail)| tail.split(':').next().map(|line| format!("{path}:{line}")))
            .or_else(|| {
                body.split_once("location=\"")
                    .and_then(|(_, tail)| tail.split_once('\"'))
                    .map(|(location, _)| location.to_string())
            })
    }

    fn cubic_thread_findings(document: &GithubDocument) -> Vec<(String, Option<String>)> {
        document
            .review_comment_sections
            .iter()
            .flat_map(|section| &section.comments)
            .filter(|comment| comment.body.contains("<!-- cubic:"))
            .flat_map(|comment| {
                let location = fixture_cubic_location(&comment.body);
                comment
                    .body
                    .lines()
                    .filter(|line| matches!(line.as_bytes(), [b'P', b'0'..=b'3', b':', ..]))
                    .map(move |finding| {
                        (
                            fixture_first_sentence(finding).to_string(),
                            location.clone(),
                        )
                    })
            })
            .collect()
    }

    fn discussion_bodies(document: &GithubDocument) -> Vec<&str> {
        document
            .comments
            .iter()
            .map(|comment| comment.body.as_str())
            .chain(
                document
                    .reviews
                    .iter()
                    .filter(|review| !review.body.is_empty())
                    .map(|review| review.body.as_str()),
            )
            .chain(
                document
                    .review_comment_sections
                    .iter()
                    .flat_map(|section| &section.comments)
                    .map(|comment| comment.body.as_str()),
            )
            .collect()
    }

    #[test]
    fn pinned_fixtures_compress_without_losing_cubic_findings_or_human_replies() {
        let document = fixture(270);
        let resource = parse_resource("pr://cortexkit/aft/270").unwrap();
        let rendered = render_document_for_resource(&document, &resource).unwrap();
        let findings = cubic_thread_findings(&document);
        assert_eq!(
            findings.len(),
            22,
            "the pinned capture's finding count changed"
        );
        assert!(
            !findings.is_empty(),
            "the cubic finding oracle visited zero rows"
        );

        let cubic_segments = rendered
            .split("### [")
            .filter(|segment| segment.contains("[cubic comment, compressed]"))
            .collect::<Vec<_>>()
            .join("\n");
        for (finding, location) in &findings {
            assert!(
                cubic_segments.contains(finding),
                "compressed cubic items lost the verbatim first sentence: {finding}"
            );
            if let Some(location) = location {
                let expected = format!("{finding} ({location})");
                assert!(
                    cubic_segments.contains(&expected),
                    "compressed cubic item lost its details-block location: {expected}"
                );
            }
        }

        let trey_replies = document
            .review_comment_sections
            .iter()
            .flat_map(|section| &section.comments)
            .filter(|comment| comment.author.as_deref() == Some("TreyThomasCodes"))
            .collect::<Vec<_>>();
        assert_eq!(
            trey_replies.len(),
            26,
            "the pinned Trey reply count changed"
        );
        for reply in trey_replies {
            assert!(
                rendered.contains(&structural_strip(&reply.body)),
                "a TreyThomasCodes reply did not survive byte-for-byte after structural stripping"
            );
        }

        let human_comments = document
            .comments
            .iter()
            .filter(|comment| comment.author.as_deref() != Some("greptile-apps"))
            .collect::<Vec<_>>();
        assert_eq!(
            human_comments.len(),
            3,
            "the pinned human comment count changed"
        );
        for comment in human_comments {
            assert!(
                rendered.contains(&structural_strip(&comment.body)),
                "a human discussion comment did not survive byte-for-byte after structural stripping"
            );
        }

        let empty_commented_reviews = document
            .reviews
            .iter()
            .filter(|review| review.state.as_deref() == Some("COMMENTED") && review.body.is_empty())
            .count();
        assert_eq!(
            empty_commented_reviews, 34,
            "the pinned empty COMMENTED review count changed"
        );
        let reviews = rendered
            .split_once("## Reviews")
            .and_then(|(_, tail)| tail.split_once("## Review comments"))
            .map(|(reviews, _)| reviews)
            .expect("fixture render has both review sections");
        assert!(!reviews.contains("@TreyThomasCodes"));
        assert!(!reviews.contains("@greptile-apps"));
        let nonempty_commented_reviews = document
            .reviews
            .iter()
            .filter(|review| {
                review.state.as_deref() == Some("COMMENTED") && !review.body.is_empty()
            })
            .count();
        assert_eq!(
            nonempty_commented_reviews, 11,
            "the visible COMMENTED review count changed"
        );
        assert_eq!(
            reviews.matches("State: COMMENTED").count(),
            nonempty_commented_reviews
        );

        let raw_codesmith_markers = PR_270_JSON.matches("<!-- codesmith:footer -->").count()
            + PR_283_JSON.matches("<!-- codesmith:footer -->").count();
        assert_eq!(raw_codesmith_markers, 2, "the pinned footer count changed");
        let rendered_283 = render_document_for_resource(
            &fixture(283),
            &parse_resource("pr://cortexkit/aft/283").unwrap(),
        )
        .unwrap();
        for compressed in [&rendered, &rendered_283] {
            assert!(!compressed.contains("codesmith:footer"));
            assert!(!compressed.contains("pr-comments-assets.blacksmith.sh"));
        }

        let prompt_blocks = document
            .review_comment_sections
            .iter()
            .flat_map(|section| &section.comments)
            .filter(|comment| {
                comment
                    .body
                    .contains("<summary>Prompt for AI agents</summary>")
            })
            .count();
        assert_eq!(prompt_blocks, 22, "the pinned prompt-block count changed");
        assert!(!rendered.contains("Prompt for AI agents"));

        assert!(rendered.contains("[greptile comment, compressed]"));
        assert!(
            rendered.contains("- Adds shared npm invocation and process-tree termination helpers.")
        );
        assert!(rendered.contains(
            "P1: Cancellation leaves npm running — When a Windows `npm.cmd` invocation is aborted or times out, callers terminate the new `cmd.exe` child and proceed as though npm has stopped, while its npm/node descendant can continue writing."
        ));
        for dropped in [
            "Confidence Score: 5/5",
            "```mermaid",
            "Knowledge Base Used:",
            "greptile-static-assets",
        ] {
            assert!(
                !rendered.contains(dropped),
                "Greptile noise survived: {dropped}"
            );
        }

        let raw_fixture_bytes = PR_270_JSON.len() + PR_270_REVIEWS_JSON.len();
        assert!(
            rendered.len() * 100 <= raw_fixture_bytes * 40,
            "compressed PR 270 render is {} bytes, above 40% of {raw_fixture_bytes} fixture bytes",
            rendered.len()
        );
    }

    #[test]
    fn selectors_return_full_structurally_stripped_items_in_the_same_address_space() {
        let document = fixture(270);
        let bodies = discussion_bodies(&document);
        let cubic_index = bodies
            .iter()
            .position(|body| {
                body.contains("<!-- cubic:v=")
                    && body
                        .lines()
                        .any(|line| matches!(line.as_bytes(), [b'P', b'0'..=b'3', b':', ..]))
            })
            .expect("fixture contains a cubic thread finding");
        let cubic_body = bodies[cubic_index];
        let default_render = render_document_for_resource(
            &document,
            &parse_resource("pr://cortexkit/aft/270").unwrap(),
        )
        .unwrap();
        let cubic_finding = fixture_first_sentence(
            cubic_body
                .lines()
                .find(|line| matches!(line.as_bytes(), [b'P', b'0'..=b'3', b':', ..]))
                .expect("fixture cubic item includes a finding"),
        );
        let rendered_cubic_item = default_render
            .split("### [")
            .find(|item| item.contains(cubic_finding))
            .expect("default render includes the compressed cubic finding");
        let cubic_ordinal = rendered_cubic_item
            .split_once(']')
            .and_then(|(ordinal, _)| ordinal.parse::<usize>().ok())
            .expect("default render item begins with its discussion ordinal");
        assert!(
            rendered_cubic_item.contains(&format!(
                "[compressed; full: pr://cortexkit/aft/270/comments/{cubic_ordinal}]"
            )),
            "the compressed default item must point to the selector ordinal that retrieves it"
        );
        let selected = render_document_for_resource(
            &document,
            &parse_resource(&format!("pr://cortexkit/aft/270/comments/{cubic_ordinal}")).unwrap(),
        )
        .unwrap();
        assert!(selected.starts_with(&format!("### [{cubic_ordinal}] @cubic-dev-ai · ")));
        assert!(selected.contains(&structural_strip(cubic_body)));
        assert!(selected.contains("<summary>Prompt for AI agents</summary>"));
        assert!(!selected.contains("[cubic comment, compressed]"));

        let cubic_review = render_document_for_resource(
            &document,
            &parse_resource("pr://cortexkit/aft/270/comments/5").unwrap(),
        )
        .unwrap();
        assert!(cubic_review.contains("All reported issues were addressed"));
        assert!(!cubic_review.contains("Re-trigger cubic"));
        assert!(!cubic_review.contains("<!-- cubic:"));
        assert!(!cubic_review.contains("[cubic review, compressed]"));

        let range = render_document_for_resource(
            &document,
            &parse_resource("pr://cortexkit/aft/270/comments/1-2").unwrap(),
        )
        .unwrap();
        assert!(range.contains("### [1] @greptile-apps ·"));
        assert!(range.contains("### [2] @TreyThomasCodes ·"));
        assert!(range.contains(&structural_strip(document.comments[0].body.as_str())));
        assert!(range.contains(&document.comments[1].body));
        assert!(!range.contains("[compressed"));

        let greptile_ordinal = bodies
            .iter()
            .position(|body| body.contains("greptile-static-assets"))
            .expect("fixture contains a Greptile priority badge")
            + 1;
        let greptile = render_document_for_resource(
            &document,
            &parse_resource(&format!(
                "pr://cortexkit/aft/270/comments/{greptile_ordinal}"
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(greptile.contains("alt=\"P1\""));
        assert!(greptile.contains("**Knowledge Base Used:**"));

        let total = bodies.len();
        assert_eq!(total, 71, "the pinned discussion-item count changed");
        let error = render_document_for_resource(
            &document,
            &parse_resource(&format!("pr://cortexkit/aft/270/comments/{}", total + 1)).unwrap(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "invalid_comment_selector");
        assert!(matches!(error, GithubReadError::InvalidCommentSelector(_)));
        assert!(error.to_string().contains("valid range is 1-71"));
    }

    #[test]
    fn html_noise_only_known_bot_body_emits_only_its_honest_label() {
        let body = "<!-- cubic:v=fixture -->\n<details><summary>Prompt for AI agents</summary>noise</details>";
        let compressed = compress_discussion_body(Some("cubic-dev-ai"), body);
        assert_eq!(compressed.body, "[cubic comment, compressed]");
        assert!(compressed.compressed);
    }

    #[test]
    fn generic_bot_fallback_keeps_only_clean_first_paragraph_and_drop_count() {
        let body = "<!-- machine -->\nFirst <b>paragraph</b>.\n\n<details><summary>Noise</summary>secret</details>\n\nSecond paragraph.";
        let compressed = compress_discussion_body(Some("service[bot]"), body);
        assert!(compressed.compressed);
        assert!(compressed
            .body
            .starts_with("[bot comment, compressed]\nFirst paragraph."));
        assert!(compressed.body.contains("[compressed: "));
        assert!(!compressed.body.contains("secret"));
        assert!(!compressed.body.contains("Second paragraph"));
    }

    #[test]
    fn bot_like_human_text_is_not_compressed_without_a_machine_marker() {
        let body = "P1: A human can discuss bot syntax verbatim.\n\n<details>Human details stay.</details>";
        let rendered = compress_discussion_body(Some("human-reviewer"), body);
        assert_eq!(rendered.body, body);
        assert!(!rendered.compressed);
    }

    #[test]
    fn tracking_only_codesmith_item_is_a_silent_structural_deletion() {
        let body = "<!-- codesmith:footer -->tracking<!-- /codesmith:footer -->";
        let rendered = compress_discussion_body(Some("codesmith-bot"), body);
        assert!(rendered.body.is_empty());
        assert!(!rendered.compressed);
    }
}
