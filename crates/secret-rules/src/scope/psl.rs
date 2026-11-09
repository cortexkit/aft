use std::sync::OnceLock;

/// Identity of the creation-time public suffix policy. Stored decode and match
/// do not depend on this snapshot, so an update cannot revoke existing scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PslSnapshot {
    pub url: &'static str,
    pub date: &'static str,
    pub sha256: &'static str,
}

pub const PSL_SNAPSHOT: PslSnapshot = PslSnapshot {
    url: "https://raw.githubusercontent.com/publicsuffix/list/ab73a956765706dc3f42dbac5c7e48df2297e1ac/public_suffix_list.dat",
    date: "2026-11-09",
    sha256: "7bd9c5479d1ac1a4118b982a3a05b65719e3215cb873474f9799af6222006da4",
};

const SOURCE: &str = include_str!("public_suffix_list.dat");
static LIST: OnceLock<PublicSuffixList> = OnceLock::new();

struct PublicSuffixList {
    exact: Vec<Box<str>>,
    wildcard: Vec<Box<str>>,
    exception: Vec<Box<str>>,
}

impl PublicSuffixList {
    fn parse() -> Self {
        let mut list = Self {
            exact: Vec::new(),
            wildcard: Vec::new(),
            exception: Vec::new(),
        };
        for line in SOURCE
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
        {
            let (rules, text) = if let Some(text) = line.strip_prefix('!') {
                (&mut list.exception, text)
            } else if let Some(text) = line.strip_prefix("*.") {
                (&mut list.wildcard, text)
            } else {
                (&mut list.exact, line)
            };
            // PSL rules include Unicode; scope hosts already use ASCII IDNA
            // domain labels (A-labels), so both sides must use that spelling.
            rules.push(
                idna::domain_to_ascii(text)
                    .expect("pinned PSL rule must be valid IDNA")
                    .to_ascii_lowercase()
                    .into_boxed_str(),
            );
        }
        for rules in [&mut list.exact, &mut list.wildcard, &mut list.exception] {
            rules.sort();
            rules.dedup();
            rules.shrink_to_fit();
        }
        list
    }

    fn is_public_suffix(&self, domain: &str) -> bool {
        let contains = |rules: &[Box<str>], value: &str| {
            rules
                .binary_search_by(|rule| rule.as_ref().cmp(value))
                .is_ok()
        };
        let labels = domain.split('.').count();
        let mut suffix_labels = 1; // The default PSL rule is `*`, even for unknown TLDs.
        let mut suffix = domain;
        let mut remaining = labels;
        loop {
            // Exceptions prevail over every ordinary or wildcard match.
            if contains(&self.exception, suffix) {
                return false;
            }
            if contains(&self.exact, suffix) {
                suffix_labels = suffix_labels.max(remaining);
            }
            if let Some((_, parent)) = suffix.split_once('.') {
                if contains(&self.wildcard, parent) {
                    suffix_labels = suffix_labels.max(remaining);
                }
                suffix = parent;
                remaining -= 1;
            } else {
                break;
            }
        }
        suffix_labels == labels
    }
}

pub(super) fn is_public_suffix(domain: &str) -> bool {
    LIST.get_or_init(PublicSuffixList::parse)
        .is_public_suffix(domain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn pinned_snapshot_bytes_and_parse_cost() {
        assert_eq!(
            format!("{:x}", Sha256::digest(SOURCE.as_bytes())),
            PSL_SNAPSHOT.sha256
        );
        let start = std::time::Instant::now();
        let list = PublicSuffixList::parse();
        let elapsed = start.elapsed();
        let sets = [&list.exact, &list.wildcard, &list.exception];
        let rules: usize = sets.iter().map(|set| set.len()).sum();
        let heap_bytes: usize = sets
            .iter()
            .map(|set| {
                set.capacity() * std::mem::size_of::<Box<str>>()
                    + set.iter().map(|s| s.len()).sum::<usize>()
            })
            .sum();
        eprintln!("PSL: {rules} rules parsed in {elapsed:?}; {heap_bytes} retained heap bytes (excluding allocator overhead), {} embedded bytes", SOURCE.len());
        assert!(rules > 10_000);
        assert!(list.is_public_suffix("github.io"));
        assert!(list.is_public_suffix("xn--55qx5d.cn"));
        assert!(list.is_public_suffix("a.ck"));
        assert!(!list.is_public_suffix("www.ck"));
        assert!(!list.is_public_suffix("a.www.ck"));
        assert!(list.is_public_suffix("unknown-tld"));
    }
}
