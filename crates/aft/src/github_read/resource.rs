use std::collections::BTreeSet;
use std::fmt;
use std::ops::RangeInclusive;

use url::Url;

/// One of the two GitHub resource kinds supported by the read scheme.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum GithubResourceKind {
    Issue,
    PullRequest,
}

impl GithubResourceKind {
    pub const fn scheme(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pr",
        }
    }

    pub const fn command(self) -> &'static str {
        match self {
            Self::Issue => "issue",
            Self::PullRequest => "pr",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Issue => "Issue",
            Self::PullRequest => "Pull request",
        }
    }
}

/// An item in a discussion comment selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectorItem {
    /// 1-based ordinal range from the start: `start..=end`.
    Positive(RangeInclusive<usize>),
    /// 1-based ordinal range from the end:
    /// `start` is distance from end (e.g. 3 for -3), `end` is distance from end (e.g. 1 for -1).
    /// In chronological order, `start >= end`.
    Negative { start: usize, end: usize },
}

/// One resolved selection of discussion ordinals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedCommentSelector {
    ranges: Vec<RangeInclusive<usize>>,
}

impl ResolvedCommentSelector {
    pub fn contains(&self, ordinal: usize) -> bool {
        self.ranges.iter().any(|range| range.contains(&ordinal))
    }

    pub fn ranges(&self) -> &[RangeInclusive<usize>] {
        &self.ranges
    }

    pub fn ordinals(&self) -> BTreeSet<usize> {
        let mut ordinals = BTreeSet::new();
        for range in &self.ranges {
            for ordinal in range.clone() {
                ordinals.insert(ordinal);
            }
        }
        ordinals
    }
}

/// Ordinals selected by a `/comments/<sel>` discussion drill-down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubCommentSelector {
    items: Vec<SelectorItem>,
}

impl GithubCommentSelector {
    /// Parse the selector grammar shared by `/comments/<sel>` and GitHub zoom.
    pub fn parse(selector: &str) -> Result<Self, InvalidGithubResource> {
        parse_comment_selector("/comments/<sel>", selector)
    }

    pub fn contains(&self, ordinal: usize) -> bool {
        self.items.iter().any(|item| match item {
            SelectorItem::Positive(range) => range.contains(&ordinal),
            SelectorItem::Negative { .. } => false,
        })
    }

    /// Resolve the selector against a discussion's total item count.
    ///
    /// Returns the resolved set of 1-based ordinal ranges, or the first ordinal
    /// (positive or negative) that was out of range.
    pub fn resolve(&self, total_count: usize) -> Result<ResolvedCommentSelector, isize> {
        let mut ranges = Vec::new();
        for item in &self.items {
            match item {
                SelectorItem::Positive(range) => {
                    if total_count == 0 {
                        return Err(*range.start() as isize);
                    }
                    if *range.start() > total_count {
                        return Err(*range.start() as isize);
                    }
                    if *range.end() > total_count {
                        return Err(*range.end() as isize);
                    }
                    ranges.push(range.clone());
                }
                SelectorItem::Negative { start, end } => {
                    if total_count == 0 || *start > total_count {
                        return Err(-(*start as isize));
                    }
                    let resolved_start = total_count - *start + 1;
                    let resolved_end = total_count - *end + 1;
                    ranges.push(resolved_start..=resolved_end);
                }
            }
        }
        Ok(ResolvedCommentSelector { ranges })
    }

    pub fn first_out_of_range(&self, valid_end: usize) -> Option<isize> {
        self.resolve(valid_end).err()
    }
}

/// A validated `issue://` or `pr://` resource.
///
/// Short resources deliberately retain no inferred repository. `gh` resolves
/// those resources from the request's working directory, while explicit
/// resources carry the exact `OWNER/REPO` argument to pass to `gh -R`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubResource {
    pub kind: GithubResourceKind,
    pub number: u64,
    pub repository: Option<String>,
    pub comment_selector: Option<GithubCommentSelector>,
}

impl GithubResource {
    pub fn is_explicit(&self) -> bool {
        self.repository.is_some()
    }

    pub fn base_spelling(&self) -> String {
        match &self.repository {
            Some(repository) => format!("{}://{repository}/{}", self.kind.scheme(), self.number),
            None => format!("{}://{}", self.kind.scheme(), self.number),
        }
    }

    pub fn without_comment_selector(&self) -> Self {
        let mut resource = self.clone();
        resource.comment_selector = None;
        resource
    }
}

/// A typed error produced before a GitHub resource can enter the fetch path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidGithubResource {
    resource: String,
    reason: String,
}

impl InvalidGithubResource {
    fn new(resource: &str, reason: impl Into<String>) -> Self {
        Self {
            resource: resource.to_string(),
            reason: reason.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        "invalid_resource"
    }
}

impl fmt::Display for InvalidGithubResource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "invalid GitHub resource '{}': {}. Use issue://NUMBER, pr://NUMBER, issue://OWNER/REPO/NUMBER, or pr://OWNER/REPO/NUMBER; append /comments/<sel> for discussion ordinals",
            self.resource, self.reason
        )
    }
}

impl std::error::Error for InvalidGithubResource {}

/// Parse only the GitHub resource forms that the read command exposes.
///
/// This rejects query strings, fragments, ports, credentials, empty path
/// components, and alternate URL shapes rather than silently treating them as
/// filesystem paths or a different remote resource.
pub fn parse_resource(resource: &str) -> Result<GithubResource, InvalidGithubResource> {
    let parsed = Url::parse(resource)
        .map_err(|_| InvalidGithubResource::new(resource, "the URL is malformed"))?;
    let kind = match parsed.scheme() {
        "issue" => GithubResourceKind::Issue,
        "pr" => GithubResourceKind::PullRequest,
        _ => return Err(InvalidGithubResource::new(resource, "unsupported scheme")),
    };

    if parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.port().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(InvalidGithubResource::new(
            resource,
            "unsupported URL authority or suffix",
        ));
    }

    let authority = parsed
        .host_str()
        .ok_or_else(|| InvalidGithubResource::new(resource, "missing authority"))?;
    let path_segments: Vec<_> = parsed
        .path_segments()
        .map(|segments| segments.collect())
        .unwrap_or_default();

    // `issue://373` uses the URL authority for the number. It has no path.
    if parsed.path().is_empty() {
        return Ok(GithubResource {
            kind,
            number: parse_number(resource, authority)?,
            repository: None,
            comment_selector: None,
        });
    }

    // A short resource keeps its number in the authority, so its only accepted
    // path is the discussion drill-down suffix.
    if authority.bytes().all(|byte| byte.is_ascii_digit()) {
        if path_segments.len() == 2 && path_segments[0] == "comments" {
            return Ok(GithubResource {
                kind,
                number: parse_number(resource, authority)?,
                repository: None,
                comment_selector: Some(parse_comment_selector(resource, path_segments[1])?),
            });
        }
        return Err(InvalidGithubResource::new(
            resource,
            "unsupported short-resource suffix",
        ));
    }

    // Explicit resources use the authority for OWNER and begin their path with
    // REPO/NUMBER, optionally followed by comments/SELECTOR.
    if !valid_repository_component(authority)
        || path_segments.len() < 2
        || !valid_repository_component(path_segments[0])
    {
        return Err(InvalidGithubResource::new(
            resource,
            "unsupported authority or malformed repository path",
        ));
    }
    let comment_selector = match path_segments.as_slice() {
        [_, _] => None,
        [_, _, "comments", selector] => Some(parse_comment_selector(resource, selector)?),
        _ => {
            return Err(InvalidGithubResource::new(
                resource,
                "unsupported authority or malformed repository path",
            ))
        }
    };

    Ok(GithubResource {
        kind,
        number: parse_number(resource, path_segments[1])?,
        repository: Some(format!("{authority}/{}", path_segments[0])),
        comment_selector,
    })
}

fn parse_number(resource: &str, value: &str) -> Result<u64, InvalidGithubResource> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(InvalidGithubResource::new(
            resource,
            "resource number must be numeric",
        ));
    }
    let number = value
        .parse::<u64>()
        .map_err(|_| InvalidGithubResource::new(resource, "resource number is out of range"))?;
    if number == 0 {
        return Err(InvalidGithubResource::new(
            resource,
            "resource number must be greater than zero",
        ));
    }
    Ok(number)
}

fn parse_comment_selector(
    resource: &str,
    selector: &str,
) -> Result<GithubCommentSelector, InvalidGithubResource> {
    if selector.is_empty() {
        return Err(InvalidGithubResource::new(
            resource,
            "comment selector is empty",
        ));
    }
    let mut items = Vec::new();
    for item in selector.split(',') {
        if item.is_empty() {
            return Err(InvalidGithubResource::new(
                resource,
                "comment selector contains an empty item",
            ));
        }
        let parsed = parse_selector_item(resource, item)?;
        items.push(parsed);
    }
    Ok(GithubCommentSelector { items })
}

fn parse_selector_item(resource: &str, item: &str) -> Result<SelectorItem, InvalidGithubResource> {
    if let Some(rest) = item.strip_prefix('-') {
        if rest.is_empty() || rest.starts_with('-') {
            return Err(InvalidGithubResource::new(
                resource,
                "comment selector ordinals must be non-zero integers (for example 3, 3-5, 3,7, or -1)",
            ));
        }
        if let Some((start_str, after_hyphen)) = rest.split_once('-') {
            let start_mag = parse_ordinal_magnitude(resource, start_str)?;
            if after_hyphen.is_empty() {
                Ok(SelectorItem::Negative {
                    start: start_mag,
                    end: 1,
                })
            } else if let Some(end_str) = after_hyphen.strip_prefix('-') {
                if end_str.contains('-') {
                    return Err(InvalidGithubResource::new(
                        resource,
                        "comment selector ranges contain exactly one hyphen",
                    ));
                }
                let end_mag = parse_ordinal_magnitude(resource, end_str)?;
                if start_mag < end_mag {
                    return Err(InvalidGithubResource::new(
                        resource,
                        "comment selector range start exceeds its end",
                    ));
                }
                Ok(SelectorItem::Negative {
                    start: start_mag,
                    end: end_mag,
                })
            } else {
                Err(InvalidGithubResource::new(
                    resource,
                    "comment selector ordinals must be non-zero integers (for example 3, 3-5, 3,7, or -1)",
                ))
            }
        } else {
            let mag = parse_ordinal_magnitude(resource, rest)?;
            Ok(SelectorItem::Negative {
                start: mag,
                end: mag,
            })
        }
    } else if let Some((start_str, end_str)) = item.split_once('-') {
        if end_str.contains('-') || end_str.starts_with('-') {
            return Err(InvalidGithubResource::new(
                resource,
                "comment selector ranges contain exactly one hyphen",
            ));
        }
        if end_str.is_empty() {
            return Err(InvalidGithubResource::new(
                resource,
                "comment selector ordinals must be non-zero integers (for example 3, 3-5, 3,7, or -1)",
            ));
        }
        let start = parse_ordinal_magnitude(resource, start_str)?;
        let end = parse_ordinal_magnitude(resource, end_str)?;
        if start > end {
            return Err(InvalidGithubResource::new(
                resource,
                "comment selector range start exceeds its end",
            ));
        }
        Ok(SelectorItem::Positive(start..=end))
    } else {
        let ordinal = parse_ordinal_magnitude(resource, item)?;
        Ok(SelectorItem::Positive(ordinal..=ordinal))
    }
}

fn parse_ordinal_magnitude(resource: &str, value: &str) -> Result<usize, InvalidGithubResource> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(InvalidGithubResource::new(
            resource,
            "comment selector ordinals must be non-zero integers (for example 3, 3-5, 3,7, or -1)",
        ));
    }
    let ordinal = value.parse::<usize>().map_err(|_| {
        InvalidGithubResource::new(resource, "comment selector ordinal is out of range")
    })?;
    if ordinal == 0 {
        return Err(InvalidGithubResource::new(
            resource,
            "comment selector ordinals must be non-zero integers (for example 3, 3-5, 3,7, or -1)",
        ));
    }
    Ok(ordinal)
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_four_resource_forms() {
        assert_eq!(
            parse_resource("issue://373").unwrap(),
            GithubResource {
                kind: GithubResourceKind::Issue,
                number: 373,
                repository: None,
                comment_selector: None,
            }
        );
        assert_eq!(
            parse_resource("pr://Owner/repo-name/45").unwrap(),
            GithubResource {
                kind: GithubResourceKind::PullRequest,
                number: 45,
                repository: Some("Owner/repo-name".to_string()),
                comment_selector: None,
            }
        );

        for value in [
            "issue:///373",
            "issue://373/",
            "issue://owner/repo/not-a-number",
            "issue://owner/repo/0",
            "issue://owner/repo/1/extra",
            "issue://owner/repo/1?view=full",
            "issue://owner:secret@repo/1",
            "issue://github.com/owner/repo/1",
            "https://github.com/owner/repo/issues/1",
        ] {
            assert!(parse_resource(value).is_err(), "{value}");
        }
    }

    #[test]
    fn parses_comment_ordinal_selectors_for_short_and_explicit_resources() {
        let short = parse_resource("issue://373/comments/3,7").unwrap();
        let short_selector = short.comment_selector.as_ref().unwrap();
        assert!(short_selector.contains(3));
        assert!(short_selector.contains(7));
        assert!(!short_selector.contains(4));
        assert_eq!(short.base_spelling(), "issue://373");

        let explicit = parse_resource("pr://Owner/repo-name/45/comments/3-5").unwrap();
        let explicit_selector = explicit.comment_selector.as_ref().unwrap();
        assert!((3..=5).all(|ordinal| explicit_selector.contains(ordinal)));
        assert_eq!(explicit.base_spelling(), "pr://Owner/repo-name/45");

        for value in [
            "pr://45/comments/0",
            "pr://45/comments/",
            "pr://45/comments/5-3",
            "pr://45/comments/3-5-7",
            "pr://45/comments/3,,7",
            "pr://owner/repo/45/comments/3/extra",
            "pr://45/comments/--1",
            "pr://45/comments/-0",
            "pr://45/comments/1--3",
        ] {
            assert!(parse_resource(value).is_err(), "{value}");
        }
    }

    #[test]
    fn comment_selector_parser_table() {
        // Positive
        for (input, expected) in [
            ("3", vec![SelectorItem::Positive(3..=3)]),
            ("3-5", vec![SelectorItem::Positive(3..=5)]),
            (
                "3,7",
                vec![SelectorItem::Positive(3..=3), SelectorItem::Positive(7..=7)],
            ),
        ] {
            let selector = GithubCommentSelector::parse(input).unwrap();
            assert_eq!(selector.items, expected, "input: {input}");
        }

        // Negative
        for (input, expected) in [
            ("-1", vec![SelectorItem::Negative { start: 1, end: 1 }]),
            ("-3", vec![SelectorItem::Negative { start: 3, end: 3 }]),
            ("-3-", vec![SelectorItem::Negative { start: 3, end: 1 }]),
            ("-3--1", vec![SelectorItem::Negative { start: 3, end: 1 }]),
            (
                "-3,-1",
                vec![
                    SelectorItem::Negative { start: 3, end: 3 },
                    SelectorItem::Negative { start: 1, end: 1 },
                ],
            ),
        ] {
            let selector = GithubCommentSelector::parse(input).unwrap();
            assert_eq!(selector.items, expected, "input: {input}");
        }

        // Mixed
        for (input, expected) in [
            (
                "2,-1",
                vec![
                    SelectorItem::Positive(2..=2),
                    SelectorItem::Negative { start: 1, end: 1 },
                ],
            ),
            (
                "-3,5",
                vec![
                    SelectorItem::Negative { start: 3, end: 3 },
                    SelectorItem::Positive(5..=5),
                ],
            ),
            (
                "1-3,-1",
                vec![
                    SelectorItem::Positive(1..=3),
                    SelectorItem::Negative { start: 1, end: 1 },
                ],
            ),
            (
                "2,-3-",
                vec![
                    SelectorItem::Positive(2..=2),
                    SelectorItem::Negative { start: 3, end: 1 },
                ],
            ),
        ] {
            let selector = GithubCommentSelector::parse(input).unwrap();
            assert_eq!(selector.items, expected, "input: {input}");
        }

        // Malformed (including explicitly required: `--1`, `-0`, `1--3`)
        for input in [
            "--1", "-0", "1--3", "0", "1-", "-3-5", "-1--3", "5-3", "3-5-7", "-3--1--2", "-",
            "---", "", "3,,7", ",1", "1,", "abc", "-abc", "1-abc", "-3--abc",
        ] {
            assert!(
                GithubCommentSelector::parse(input).is_err(),
                "malformed input must be rejected: {input}"
            );
        }
    }

    #[test]
    fn comment_selector_resolution_against_5_item_fixture() {
        // -1 -> 5
        let selector = GithubCommentSelector::parse("-1").unwrap();
        let resolved = selector.resolve(5).unwrap();
        assert_eq!(resolved.ordinals(), BTreeSet::from([5]));
        assert_eq!(resolved.ranges(), &[5..=5]);

        // -3- -> 3..=5
        let selector = GithubCommentSelector::parse("-3-").unwrap();
        let resolved = selector.resolve(5).unwrap();
        assert_eq!(resolved.ranges(), &[3..=5]);
        assert_eq!(resolved.ordinals(), BTreeSet::from([3, 4, 5]));

        // -3--1 -> 3..=5
        let selector = GithubCommentSelector::parse("-3--1").unwrap();
        let resolved = selector.resolve(5).unwrap();
        assert_eq!(resolved.ranges(), &[3..=5]);

        // 2,-1 -> {2, 5}
        let selector = GithubCommentSelector::parse("2,-1").unwrap();
        let resolved = selector.resolve(5).unwrap();
        assert_eq!(resolved.ordinals(), BTreeSet::from([2, 5]));
        assert!(resolved.contains(2));
        assert!(resolved.contains(5));
        assert!(!resolved.contains(1));
        assert!(!resolved.contains(3));
        assert!(!resolved.contains(4));

        // -9 error
        let selector = GithubCommentSelector::parse("-9").unwrap();
        let err = selector.resolve(5).unwrap_err();
        assert_eq!(err, -9);

        // Positive out of range error
        let selector = GithubCommentSelector::parse("9").unwrap();
        let err = selector.resolve(5).unwrap_err();
        assert_eq!(err, 9);

        // Empty discussion error
        let selector = GithubCommentSelector::parse("-1").unwrap();
        let err = selector.resolve(0).unwrap_err();
        assert_eq!(err, -1);
    }
}
