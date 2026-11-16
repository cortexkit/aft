//! Build-domain identifiers and suspension facts shared with durable readers.

/// Every expensive background build must choose one explicit domain. The enum is
/// intentionally exhaustive so new schedulers cannot silently bypass the breaker.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BuildDomain {
    CallgraphCold,
    SearchCold,
    SemanticSeed,
    Tier2Scan,
}

impl BuildDomain {
    pub const ALL: [Self; 4] = [
        Self::CallgraphCold,
        Self::SearchCold,
        Self::SemanticSeed,
        Self::Tier2Scan,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CallgraphCold => "callgraph_cold",
            Self::SearchCold => "search_cold",
            Self::SemanticSeed => "semantic_seed",
            Self::Tier2Scan => "tier2_scan",
        }
    }

    pub fn from_persisted(value: &str) -> Option<Self> {
        match value {
            "callgraph_cold" => Some(Self::CallgraphCold),
            "search_cold" => Some(Self::SearchCold),
            "semantic_seed" => Some(Self::SemanticSeed),
            "tier2_scan" => Some(Self::Tier2Scan),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildSuspension {
    pub domain: BuildDomain,
    pub reason: String,
    pub death_count: u64,
    pub suspended_since_unix_ms: u64,
}

impl BuildSuspension {
    pub fn age_millis_at(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.suspended_since_unix_ms)
    }

    pub fn age_seconds_at(&self, now_ms: u64) -> u64 {
        self.age_millis_at(now_ms) / 1_000
    }
}
