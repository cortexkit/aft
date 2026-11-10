use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectCategory {
    Diagnostics,
    Metrics,
    Todos,
    DeadCode,
    UnusedExports,
    Duplicates,
    Cycles,
    Complexity,
    CircularDeps,
    OutdatedDeps,
    Vulnerabilities,
    TestCoverageGaps,
    ApiSurface,
}

impl InspectCategory {
    pub const ACTIVE: [InspectCategory; 8] = [
        InspectCategory::Diagnostics,
        InspectCategory::Metrics,
        InspectCategory::Todos,
        InspectCategory::DeadCode,
        InspectCategory::UnusedExports,
        InspectCategory::Duplicates,
        InspectCategory::Cycles,
        InspectCategory::Complexity,
    ];

    pub const DISABLED: [InspectCategory; 5] = [
        InspectCategory::CircularDeps,
        InspectCategory::OutdatedDeps,
        InspectCategory::Vulnerabilities,
        InspectCategory::TestCoverageGaps,
        InspectCategory::ApiSurface,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            InspectCategory::Diagnostics => "diagnostics",
            InspectCategory::Metrics => "metrics",
            InspectCategory::Todos => "todos",
            InspectCategory::DeadCode => "dead_code",
            InspectCategory::UnusedExports => "unused_exports",
            InspectCategory::Duplicates => "duplicates",
            InspectCategory::Cycles => "cycles",
            InspectCategory::Complexity => "complexity",
            InspectCategory::CircularDeps => "circular_deps",
            InspectCategory::OutdatedDeps => "outdated_deps",
            InspectCategory::Vulnerabilities => "vulnerabilities",
            InspectCategory::TestCoverageGaps => "test_coverage_gaps",
            InspectCategory::ApiSurface => "api_surface",
        }
    }

    pub fn tier(self) -> InspectTier {
        match self {
            InspectCategory::Diagnostics | InspectCategory::Metrics | InspectCategory::Todos => {
                InspectTier::Tier1
            }
            InspectCategory::DeadCode
            | InspectCategory::UnusedExports
            | InspectCategory::Duplicates
            | InspectCategory::Cycles
            | InspectCategory::Complexity
            | InspectCategory::CircularDeps
            | InspectCategory::ApiSurface => InspectTier::Tier2,
            InspectCategory::OutdatedDeps
            | InspectCategory::Vulnerabilities
            | InspectCategory::TestCoverageGaps => InspectTier::Tier3,
        }
    }

    pub fn is_tier2(self) -> bool {
        self.tier() == InspectTier::Tier2
    }

    pub fn is_active(self) -> bool {
        Self::ACTIVE.contains(&self)
    }

    pub fn active() -> &'static [InspectCategory] {
        &Self::ACTIVE
    }

    pub fn disabled() -> &'static [InspectCategory] {
        &Self::DISABLED
    }
}

impl fmt::Display for InspectCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for InspectCategory {
    type Err = InspectCategoryParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "diagnostics" => Ok(Self::Diagnostics),
            "metrics" => Ok(Self::Metrics),
            "todos" => Ok(Self::Todos),
            "dead_code" => Ok(Self::DeadCode),
            "unused_exports" => Ok(Self::UnusedExports),
            "duplicates" => Ok(Self::Duplicates),
            "cycles" => Ok(Self::Cycles),
            "complexity" => Ok(Self::Complexity),
            "circular_deps" => Ok(Self::CircularDeps),
            "outdated_deps" => Ok(Self::OutdatedDeps),
            "vulnerabilities" => Ok(Self::Vulnerabilities),
            "test_coverage_gaps" => Ok(Self::TestCoverageGaps),
            "api_surface" => Ok(Self::ApiSurface),
            other => Err(InspectCategoryParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectCategoryParseError(String);

impl fmt::Display for InspectCategoryParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown inspect category '{}'", self.0)
    }
}

impl std::error::Error for InspectCategoryParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectTier {
    Tier1,
    Tier2,
    Tier3,
}
