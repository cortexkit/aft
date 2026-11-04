//! Turning a regular expression into a trigram query.
//!
//! The planner follows the analysis of Google Code Search ("Regular
//! Expression Matching with a Trigram Index", Russ Cox, 2012): for each part
//! of the parsed expression it tracks the exact strings the part can match
//! when there are few, otherwise the possible prefixes and suffixes, plus a
//! trigram query every match must satisfy. Concatenations combine a part's
//! suffixes with the next part's prefixes, so a run of literal text that
//! crosses group or class boundaries still yields its trigrams.
//!
//! All strings are folded the way the index folds contents (ASCII letters to
//! lower case), so a case-insensitive pattern plans as well as a
//! case-sensitive one. A character class becomes a set of short strings, so
//! `[Ff]oo` and `(?i)foo` both plan as `foo`, and `(?i)k`, which also matches
//! the Kelvin sign, plans as either spelling.
//!
//! The query is a superset rule: any content the pattern matches satisfies
//! it. Contents that satisfy it are verified against the real bytes by the
//! caller; the query never decides a match.

use std::collections::BTreeSet;

use regex_syntax::hir::{Class, Hir, HirKind};

use super::trigrams::{ContentTrigrams, fold, next_bit, pack};

/// Exact string sets larger than this become prefix and suffix sets.
const MAX_EXACT: usize = 7;
/// Prefix and suffix sets larger than this are cut to shorter strings.
const MAX_SET: usize = 20;
/// A class with more characters than this could be any character.
const MAX_CLASS_CHARS: u32 = 100;

/// A boolean query over trigrams that every match of a pattern satisfies.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Query {
    /// No content can match.
    None,
    /// Every content may match: the trigrams say nothing.
    All,
    /// The content has `trigram`, followed somewhere by a byte whose mask bit
    /// is in `next` (zero: followed by anything, or by nothing).
    Trigram {
        trigram: u32,
        next: u8,
    },
    And(Vec<Query>),
    Or(Vec<Query>),
}

impl Query {
    /// Whether a content with these trigrams satisfies the query.
    #[must_use]
    pub fn matches(&self, content: &ContentTrigrams) -> bool {
        match self {
            Self::None => false,
            Self::All => true,
            Self::Trigram { trigram, next } => content
                .get(*trigram)
                .is_some_and(|mask| *next == 0 || mask & next != 0),
            Self::And(parts) => parts.iter().all(|part| part.matches(content)),
            Self::Or(parts) => parts.iter().any(|part| part.matches(content)),
        }
    }

    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::None, _) | (_, Self::None) => Self::None,
            (Self::All, query) | (query, Self::All) => query,
            (Self::And(mut left), Self::And(right)) => {
                left.extend(right);
                Self::And(sorted(left))
            }
            (Self::And(mut parts), query) | (query, Self::And(mut parts)) => {
                parts.push(query);
                Self::And(sorted(parts))
            }
            (left, right) => Self::And(sorted(vec![left, right])),
        }
    }

    fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::All, _) | (_, Self::All) => Self::All,
            (Self::None, query) | (query, Self::None) => query,
            (Self::Or(mut left), Self::Or(right)) => {
                left.extend(right);
                Self::Or(sorted(left))
            }
            (Self::Or(mut parts), query) | (query, Self::Or(mut parts)) => {
                parts.push(query);
                Self::Or(sorted(parts))
            }
            (left, right) if left == right => left,
            (left, right) => Self::Or(sorted(vec![left, right])),
        }
    }
}

fn sorted(mut parts: Vec<Query>) -> Vec<Query> {
    parts.sort();
    parts.dedup();
    parts
}

/// Plans an expression parsed with the same options the caller matches
/// with.
#[must_use]
pub fn plan(hir: &Hir) -> Query {
    let mut info = analyze(hir);
    info.simplify(true);
    info.add_exact();
    info.matches
}

/// Parses `pattern` as the regular expression engine does by default, then
/// plans it.
///
/// # Errors
///
/// When the pattern does not parse.
pub fn plan_pattern(
    pattern: &str,
    case_insensitive: bool,
) -> Result<Query, Box<regex_syntax::Error>> {
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .build()
        .parse(pattern)
        .map_err(Box::new)?;
    Ok(plan(&hir))
}

type Strings = BTreeSet<Vec<u8>>;

/// What the planner knows about the matches of one part of an expression.
#[derive(Clone, Debug)]
struct Info {
    /// Whether the part can match the empty string.
    can_empty: bool,
    /// Every string the part can match, when that set is small. Otherwise
    /// `None`, and `prefix` and `suffix` describe the matches instead.
    exact: Option<Strings>,
    /// Every match starts with one of these.
    prefix: Strings,
    /// Every match ends with one of these.
    suffix: Strings,
    /// A query every match satisfies.
    matches: Query,
}

impl Info {
    fn any_char() -> Self {
        Self {
            can_empty: false,
            exact: None,
            prefix: Strings::from([Vec::new()]),
            suffix: Strings::from([Vec::new()]),
            matches: Query::All,
        }
    }

    fn empty_string() -> Self {
        Self {
            can_empty: true,
            exact: Some(Strings::from([Vec::new()])),
            prefix: Strings::new(),
            suffix: Strings::new(),
            matches: Query::All,
        }
    }

    fn no_match() -> Self {
        Self {
            can_empty: false,
            exact: Some(Strings::new()),
            prefix: Strings::new(),
            suffix: Strings::new(),
            matches: Query::None,
        }
    }

    fn star() -> Self {
        Self {
            can_empty: true,
            ..Self::any_char()
        }
    }

    fn exact(strings: Strings) -> Self {
        Self {
            can_empty: strings.contains(&Vec::new()),
            exact: Some(strings),
            prefix: Strings::new(),
            suffix: Strings::new(),
            matches: Query::All,
        }
    }

    /// Adds the trigrams of the exact strings to the query, when every one
    /// is long enough to have one.
    fn add_exact(&mut self) {
        if let Some(exact) = &self.exact
            && min_len(exact) >= 3
        {
            let query = or_trigrams(exact);
            self.matches = std::mem::replace(&mut self.matches, Query::All).and(query);
        }
    }

    /// Keeps the sets small. With `force`, exact strings long enough to
    /// carry trigrams become part of the query.
    fn simplify(&mut self, force: bool) {
        if let Some(exact) = &self.exact {
            let shortest = min_len(exact);
            if exact.len() > MAX_EXACT || (shortest >= 3 && force) || shortest >= 4 {
                self.add_exact();
                let exact = self.exact.take().unwrap_or_default();
                for string in exact {
                    if string.len() < 3 {
                        self.prefix.insert(string.clone());
                        self.suffix.insert(string);
                    } else {
                        self.prefix.insert(string[..2].to_vec());
                        self.suffix.insert(string[string.len() - 2..].to_vec());
                    }
                }
            }
        }
        if self.exact.is_none() {
            let prefix = std::mem::take(&mut self.prefix);
            self.prefix = self.simplify_set(prefix, false);
            let suffix = std::mem::take(&mut self.suffix);
            self.suffix = self.simplify_set(suffix, true);
        }
    }

    /// Records a prefix or suffix set's trigrams in the query, then keeps
    /// only the two bytes next to the boundary, which is all a neighbour
    /// needs to form a trigram across it.
    fn simplify_set(&mut self, set: Strings, suffix: bool) -> Strings {
        self.matches = std::mem::replace(&mut self.matches, Query::All).and(or_trigrams(&set));
        let mut keep = 2;
        let mut set = cut(&set, keep, suffix);
        while set.len() > MAX_SET && keep > 0 {
            keep -= 1;
            set = cut(&set, keep, suffix);
        }
        // Knowing a match starts with "ab" says nothing more when it could
        // also start with "a": the longer string is redundant.
        let redundant = set
            .iter()
            .filter(|string| {
                set.iter().any(|other| {
                    other.len() < string.len()
                        && if suffix {
                            string.ends_with(other)
                        } else {
                            string.starts_with(other)
                        }
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        for string in redundant {
            set.remove(&string);
        }
        set
    }
}

fn analyze(hir: &Hir) -> Info {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Info::empty_string(),
        HirKind::Literal(literal) => Info::exact(Strings::from([folded(&literal.0)])),
        HirKind::Class(class) => class_info(class),
        HirKind::Capture(capture) => analyze(&capture.sub),
        HirKind::Repetition(repetition) => match (repetition.min, repetition.max) {
            (0, Some(1)) => alternate(Info::empty_string(), analyze(&repetition.sub)),
            (0, _) => Info::star(),
            (_, Some(1)) => analyze(&repetition.sub),
            _ => concat(analyze(&repetition.sub), Info::star()),
        },
        HirKind::Concat(parts) => parts.iter().fold(Info::empty_string(), |info, part| {
            concat(info, analyze(part))
        }),
        HirKind::Alternation(parts) => parts.iter().fold(Info::no_match(), |info, part| {
            alternate(info, analyze(part))
        }),
    }
}

fn class_info(class: &Class) -> Info {
    let mut strings = Strings::new();
    match class {
        Class::Unicode(class) => {
            let chars: u32 = class
                .ranges()
                .iter()
                .map(|range| u32::from(range.end()) - u32::from(range.start()) + 1)
                .sum();
            if chars > MAX_CLASS_CHARS {
                return Info::any_char();
            }
            for range in class.ranges() {
                for c in range.start()..=range.end() {
                    let mut buffer = [0; 4];
                    strings.insert(folded(c.encode_utf8(&mut buffer).as_bytes()));
                }
            }
        }
        Class::Bytes(class) => {
            let bytes: u32 = class
                .ranges()
                .iter()
                .map(|range| u32::from(range.end()) - u32::from(range.start()) + 1)
                .sum();
            if bytes > MAX_CLASS_CHARS {
                return Info::any_char();
            }
            for range in class.ranges() {
                for byte in range.start()..=range.end() {
                    strings.insert(vec![fold(byte)]);
                }
            }
        }
    }
    if strings.is_empty() {
        return Info::no_match();
    }
    let mut info = Info::exact(strings);
    info.simplify(false);
    info
}

fn concat(x: Info, y: Info) -> Info {
    let mut xy = Info {
        can_empty: x.can_empty && y.can_empty,
        exact: None,
        prefix: Strings::new(),
        suffix: Strings::new(),
        matches: x.matches.clone().and(y.matches.clone()),
    };
    if let (Some(x_exact), Some(y_exact)) = (&x.exact, &y.exact) {
        xy.exact = Some(cross(x_exact, y_exact));
    } else {
        if let Some(x_exact) = &x.exact {
            xy.prefix = cross(x_exact, &y.prefix);
        } else {
            xy.prefix.clone_from(&x.prefix);
            if x.can_empty {
                xy.prefix.extend(y.prefix.iter().cloned());
            }
        }
        if let Some(y_exact) = &y.exact {
            xy.suffix = cross(&x.suffix, y_exact);
        } else {
            xy.suffix.clone_from(&y.suffix);
            if y.can_empty {
                xy.suffix.extend(x.suffix.iter().cloned());
            }
        }
    }
    // Text crossing the boundary: a suffix of x followed by a prefix of y.
    if x.exact.is_none()
        && y.exact.is_none()
        && x.suffix.len() <= MAX_SET
        && y.prefix.len() <= MAX_SET
        && min_len(&x.suffix) + min_len(&y.prefix) >= 3
    {
        xy.matches = xy.matches.and(or_trigrams(&cross(&x.suffix, &y.prefix)));
    }
    xy.simplify(false);
    xy
}

fn alternate(mut x: Info, mut y: Info) -> Info {
    let mut xy = Info {
        can_empty: x.can_empty || y.can_empty,
        exact: None,
        prefix: Strings::new(),
        suffix: Strings::new(),
        matches: Query::All,
    };
    match (&x.exact, &y.exact) {
        (Some(x_exact), Some(y_exact)) => {
            xy.exact = Some(x_exact.union(y_exact).cloned().collect());
        }
        (Some(x_exact), None) => {
            xy.prefix = x_exact.union(&y.prefix).cloned().collect();
            xy.suffix = x_exact.union(&y.suffix).cloned().collect();
            x.add_exact();
        }
        (None, Some(y_exact)) => {
            xy.prefix = x.prefix.union(y_exact).cloned().collect();
            xy.suffix = x.suffix.union(y_exact).cloned().collect();
            y.add_exact();
        }
        (None, None) => {
            xy.prefix = x.prefix.union(&y.prefix).cloned().collect();
            xy.suffix = x.suffix.union(&y.suffix).cloned().collect();
        }
    }
    xy.matches = x.matches.or(y.matches);
    xy.simplify(false);
    xy
}

fn folded(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|byte| fold(*byte)).collect()
}

fn min_len(strings: &Strings) -> usize {
    strings.iter().map(Vec::len).min().unwrap_or(0)
}

fn cross(left: &Strings, right: &Strings) -> Strings {
    let mut out = Strings::new();
    for a in left {
        for b in right {
            let mut joined = a.clone();
            joined.extend_from_slice(b);
            out.insert(joined);
        }
    }
    out
}

/// Each string cut to its first (or, for suffixes, last) `keep` bytes.
fn cut(strings: &Strings, keep: usize, suffix: bool) -> Strings {
    strings
        .iter()
        .map(|string| {
            if string.len() <= keep {
                string.clone()
            } else if suffix {
                string[string.len() - keep..].to_vec()
            } else {
                string[..keep].to_vec()
            }
        })
        .collect()
}

/// A query that one of `strings` occurs: for each string, all of its
/// trigrams. Says nothing when a string is too short to have one.
fn or_trigrams(strings: &Strings) -> Query {
    if strings.is_empty() || min_len(strings) < 3 {
        return Query::All;
    }
    strings.iter().fold(Query::None, |query, string| {
        let all = (0..=string.len() - 3).fold(Query::All, |query, start| {
            query.and(Query::Trigram {
                trigram: pack(string[start], string[start + 1], string[start + 2]),
                next: string.get(start + 3).map_or(0, |next| next_bit(*next)),
            })
        });
        query.or(all)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::TestRng;

    fn trigram(text: &str, next: Option<u8>) -> Query {
        let bytes = text.as_bytes();
        Query::Trigram {
            trigram: pack(bytes[0], bytes[1], bytes[2]),
            next: next.map_or(0, next_bit),
        }
    }

    #[test]
    fn literals_alternations_and_gaps() {
        assert_eq!(plan_pattern("abc", false).unwrap(), trigram("abc", None));
        assert_eq!(
            plan_pattern("abcd", false).unwrap(),
            Query::And(sorted(vec![
                trigram("abc", Some(b'd')),
                trigram("bcd", None)
            ]))
        );
        assert_eq!(
            plan_pattern("foo|bar", false).unwrap(),
            Query::Or(sorted(vec![trigram("foo", None), trigram("bar", None)]))
        );
        assert_eq!(
            plan_pattern("foo.*bar", false).unwrap(),
            Query::And(sorted(vec![trigram("foo", None), trigram("bar", None)]))
        );
        assert_eq!(plan_pattern("ab", false).unwrap(), Query::All);
        assert_eq!(plan_pattern(".*", false).unwrap(), Query::All);
        assert_eq!(plan_pattern("[a&&b]", false).unwrap(), Query::None);
    }

    #[test]
    fn case_insensitive_patterns_still_plan() {
        assert_eq!(
            plan_pattern("(?i)foo", false).unwrap(),
            trigram("foo", None)
        );
        assert_eq!(plan_pattern("FOO", true).unwrap(), trigram("foo", None));
        assert_eq!(plan_pattern("[Ff]oo", false).unwrap(), trigram("foo", None));
        // `(?i)k` also matches the Kelvin sign: either spelling may occur.
        let kind = plan_pattern("(?i)kind", false).unwrap();
        assert!(matches!(kind, Query::Or(_)), "{kind:?}");
    }

    #[test]
    fn runs_cross_groups_classes_and_boundaries() {
        assert_eq!(
            plan_pattern(r"\bfoo\b", false).unwrap(),
            trigram("foo", None)
        );
        assert_eq!(
            plan_pattern("(fo)o", false).unwrap(),
            plan_pattern("foo", false).unwrap()
        );
        let nested = plan_pattern(r"\b(get|set)_value\b", false).unwrap();
        assert!(matches!(nested, Query::Or(_) | Query::And(_)), "{nested:?}");
        assert_ne!(nested, Query::All);
    }

    /// Any content a pattern matches satisfies its query.
    #[test]
    fn every_match_satisfies_the_query() {
        const PIECES: &[&str] = &[
            "foo", "Bar", "baz", "é", "K", "k", "\u{212A}", "ab", "x", "_", "get", "set", "Value",
            "\n", " ", "(", "1", "ſ", "S",
        ];
        const ATOMS: &[&str] = &[
            "foo",
            "bar",
            "Baz",
            "é",
            "k",
            "ab",
            "x",
            "[Ff]",
            "[a-c]",
            r"\w",
            ".",
            r"\d",
            "(foo|ba)",
            "(get|set)",
            "[^x]",
            r"\b",
            "^",
            "$",
            "s",
            "_",
        ];
        const SUFFIXES: &[&str] = &["", "", "", "*", "+", "?", "{2}", "{1,3}"];
        let mut rng = TestRng::new(7);
        let mut checked = 0;
        let mut planned = 0;
        for _ in 0..30_000 {
            let mut pattern = String::new();
            if rng.below(4) == 0 {
                pattern.push_str("(?i)");
            }
            let parts = 1 + rng.below_usize(4);
            for part in 0..parts {
                if part > 0 && rng.below(5) == 0 {
                    pattern.push('|');
                }
                pattern.push_str(ATOMS[rng.below_usize(ATOMS.len())]);
                pattern.push_str(SUFFIXES[rng.below_usize(SUFFIXES.len())]);
            }
            let Ok(regex) = regex::bytes::Regex::new(&pattern) else {
                continue;
            };
            let query = plan_pattern(&pattern, false).unwrap();
            if query != Query::All {
                planned += 1;
            }
            for _ in 0..8 {
                let mut text = String::new();
                for _ in 0..rng.below_usize(8) {
                    text.push_str(PIECES[rng.below_usize(PIECES.len())]);
                }
                if regex.is_match(text.as_bytes()) {
                    checked += 1;
                    assert!(
                        query.matches(&ContentTrigrams::of(text.as_bytes())),
                        "pattern {pattern:?} matches {text:?} but its query {query:?} refuses it"
                    );
                }
            }
        }
        assert!(checked > 10_000, "only {checked} matching texts checked");
        assert!(
            planned > 5_000,
            "only {planned} patterns planned any trigram"
        );
    }
}
