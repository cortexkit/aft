use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Shapes recognized by the search ranking engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchShape {
    Identifier,
    CodeLiteral,
    Short,
    NaturalLanguage,
    LogExcerpt,
    Path,
    Regex,
}

impl SearchShape {
    pub const ALL: [SearchShape; 7] = [
        SearchShape::Identifier,
        SearchShape::CodeLiteral,
        SearchShape::Short,
        SearchShape::NaturalLanguage,
        SearchShape::LogExcerpt,
        SearchShape::Path,
        SearchShape::Regex,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            SearchShape::Identifier => "identifier",
            SearchShape::CodeLiteral => "code_literal",
            SearchShape::Short => "short",
            SearchShape::NaturalLanguage => "nl",
            SearchShape::LogExcerpt => "log_excerpt",
            SearchShape::Path => "path",
            SearchShape::Regex => "regex",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "identifier" => Some(SearchShape::Identifier),
            "code_literal" => Some(SearchShape::CodeLiteral),
            "short" | "mixed" => Some(SearchShape::Short),
            "nl" | "natural_language" => Some(SearchShape::NaturalLanguage),
            "log_excerpt" | "error_code" => Some(SearchShape::LogExcerpt),
            "path" => Some(SearchShape::Path),
            "regex" => Some(SearchShape::Regex),
            _ => None,
        }
    }
}

impl fmt::Display for SearchShape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Lanes participating in search ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchLaneKind {
    Symbol,
    Exact,
    Anchored,
    Lexical,
    Variants,
    Semantic,
    PathLookup,
    FallbackWalk,
    ReadinessDisclosure,
}

impl SearchLaneKind {
    pub const ALL: [SearchLaneKind; 9] = [
        SearchLaneKind::Symbol,
        SearchLaneKind::Exact,
        SearchLaneKind::Anchored,
        SearchLaneKind::Lexical,
        SearchLaneKind::Variants,
        SearchLaneKind::Semantic,
        SearchLaneKind::PathLookup,
        SearchLaneKind::FallbackWalk,
        SearchLaneKind::ReadinessDisclosure,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            SearchLaneKind::Symbol => "symbol",
            SearchLaneKind::Exact => "exact",
            SearchLaneKind::Anchored => "anchored",
            SearchLaneKind::Lexical => "lexical",
            SearchLaneKind::Variants => "variants",
            SearchLaneKind::Semantic => "semantic",
            SearchLaneKind::PathLookup => "path_lookup",
            SearchLaneKind::FallbackWalk => "fallback_walk",
            SearchLaneKind::ReadinessDisclosure => "readiness_disclosure",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "symbol" => Some(SearchLaneKind::Symbol),
            "exact" => Some(SearchLaneKind::Exact),
            "anchored" => Some(SearchLaneKind::Anchored),
            "lexical" => Some(SearchLaneKind::Lexical),
            "variants" => Some(SearchLaneKind::Variants),
            "semantic" => Some(SearchLaneKind::Semantic),
            "path_lookup" => Some(SearchLaneKind::PathLookup),
            "fallback_walk" => Some(SearchLaneKind::FallbackWalk),
            "readiness_disclosure" => Some(SearchLaneKind::ReadinessDisclosure),
            _ => None,
        }
    }

    pub fn default_plan_order_index(&self) -> usize {
        match self {
            SearchLaneKind::Symbol => 0,
            SearchLaneKind::Exact => 1,
            SearchLaneKind::Anchored => 2,
            SearchLaneKind::Lexical => 3,
            SearchLaneKind::Variants => 4,
            SearchLaneKind::Semantic => 5,
            SearchLaneKind::PathLookup => 6,
            SearchLaneKind::FallbackWalk => 7,
            SearchLaneKind::ReadinessDisclosure => 8,
        }
    }

    pub const fn is_scored(self) -> bool {
        matches!(self, Self::Lexical | Self::Semantic)
    }
}

impl fmt::Display for SearchLaneKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A plan entry for a specific (shape, lane) pair.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LanePlanEntry {
    pub weight: Option<f32>,
    pub rrf_constant: Option<f32>,
    pub plan_order_index: usize,
}

/// Shape-indexed plan table over the complete (shape, lane) cross product.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanTable {
    pub entries: BTreeMap<String, BTreeMap<String, LanePlanEntry>>,
}

/// Startup or verification error naming the (shape, lane, field) triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTableError {
    pub shape: String,
    pub lane: String,
    pub field: String,
    pub message: String,
}

impl fmt::Display for PlanTableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "plan table error for ({}, {}, {}): {}",
            self.shape, self.lane, self.field, self.message
        )
    }
}

impl std::error::Error for PlanTableError {}

impl PlanTable {
    /// Construct the authoritative running plan table from code constants.
    pub fn running_table() -> Self {
        let mut entries = BTreeMap::new();

        for shape in SearchShape::ALL {
            let shape_str = shape.as_str().to_string();
            let mut lane_map = BTreeMap::new();

            let weights = match shape {
                SearchShape::Identifier | SearchShape::Short => (0.8f32, 0.2f32),
                SearchShape::CodeLiteral | SearchShape::LogExcerpt | SearchShape::Path => {
                    (0.9f32, 0.1f32)
                }
                SearchShape::NaturalLanguage => (0.4f32, 0.6f32),
                SearchShape::Regex => (1.0f32, 0.0f32),
            };

            for lane in SearchLaneKind::ALL {
                let weight = match lane {
                    SearchLaneKind::Lexical => Some(weights.0),
                    SearchLaneKind::Semantic => Some(weights.1),
                    _ => None,
                };
                lane_map.insert(
                    lane.as_str().to_string(),
                    LanePlanEntry {
                        weight,
                        rrf_constant: weight.map(|_| 60.0),
                        plan_order_index: lane.default_plan_order_index(),
                    },
                );
            }

            entries.insert(shape_str, lane_map);
        }

        Self { entries }
    }

    /// Load pinned plan table from a JSON string.
    pub fn from_json(json_str: &str) -> Result<Self, PlanTableError> {
        serde_json::from_str(json_str).map_err(|e| PlanTableError {
            shape: "all".to_string(),
            lane: "all".to_string(),
            field: "json".to_string(),
            message: format!("failed to parse plan-table.json: {e}"),
        })
    }

    /// Load pinned plan table from a file path.
    pub fn from_file(path: &Path) -> Result<Self, PlanTableError> {
        let content = std::fs::read_to_string(path).map_err(|e| PlanTableError {
            shape: "all".to_string(),
            lane: "all".to_string(),
            field: "file".to_string(),
            message: format!("failed to read {}: {e}", path.display()),
        })?;
        Self::from_json(&content)
    }

    /// Verify this plan table against another (e.g. running table against pinned table).
    ///
    /// Asserts:
    /// - Complete (shape, lane) cross product: missing pair or extra pair is an error.
    /// - Every exact lane MUST have weight == None and rrf_constant == None.
    /// - Every weight, rrf_constant, and plan_order_index matches.
    ///
    /// Every error names the (shape, lane, field) triple.
    pub fn verify_against(&self, pinned: &PlanTable) -> Result<(), PlanTableError> {
        // Check for missing shapes or lanes in pinned relative to self (running)
        for shape in SearchShape::ALL {
            let shape_str = shape.as_str();
            let running_lanes = match self.entries.get(shape_str) {
                Some(l) => l,
                None => {
                    return Err(PlanTableError {
                        shape: shape_str.to_string(),
                        lane: "all".to_string(),
                        field: "pair".to_string(),
                        message: "shape missing from running table".to_string(),
                    })
                }
            };

            let pinned_lanes = match pinned.entries.get(shape_str) {
                Some(l) => l,
                None => {
                    return Err(PlanTableError {
                        shape: shape_str.to_string(),
                        lane: "all".to_string(),
                        field: "pair".to_string(),
                        message: "shape missing from pinned table".to_string(),
                    })
                }
            };

            for lane in SearchLaneKind::ALL {
                let lane_str = lane.as_str();
                let running_entry = match running_lanes.get(lane_str) {
                    Some(e) => e,
                    None => {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "pair".to_string(),
                            message: "pair missing from running table".to_string(),
                        })
                    }
                };

                let pinned_entry = match pinned_lanes.get(lane_str) {
                    Some(e) => e,
                    None => {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "pair".to_string(),
                            message: "pair missing from pinned table".to_string(),
                        })
                    }
                };

                // Exact lane MUST have null weight and null rrf_constant
                if lane == SearchLaneKind::Exact {
                    if pinned_entry.weight.is_some() {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "weight".to_string(),
                            message: format!(
                                "exact lane must have null weight, got {:?}",
                                pinned_entry.weight
                            ),
                        });
                    }
                    if pinned_entry.rrf_constant.is_some() {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "rrf_constant".to_string(),
                            message: format!(
                                "exact lane must have null rrf_constant, got {:?}",
                                pinned_entry.rrf_constant
                            ),
                        });
                    }
                }

                // Check weight equality
                match (running_entry.weight, pinned_entry.weight) {
                    (Some(w_run), Some(w_pin)) => {
                        if (w_run - w_pin).abs() > 1e-6 {
                            return Err(PlanTableError {
                                shape: shape_str.to_string(),
                                lane: lane_str.to_string(),
                                field: "weight".to_string(),
                                message: format!(
                                    "weight mismatch: running={w_run}, pinned={w_pin}"
                                ),
                            });
                        }
                    }
                    (None, None) => {}
                    (r, p) => {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "weight".to_string(),
                            message: format!("weight mismatch: running={r:?}, pinned={p:?}"),
                        });
                    }
                }

                // Check rrf_constant equality
                match (running_entry.rrf_constant, pinned_entry.rrf_constant) {
                    (Some(k_run), Some(k_pin)) => {
                        if (k_run - k_pin).abs() > 1e-6 {
                            return Err(PlanTableError {
                                shape: shape_str.to_string(),
                                lane: lane_str.to_string(),
                                field: "rrf_constant".to_string(),
                                message: format!(
                                    "rrf_constant mismatch: running={k_run}, pinned={k_pin}"
                                ),
                            });
                        }
                    }
                    (None, None) => {}
                    (r, p) => {
                        return Err(PlanTableError {
                            shape: shape_str.to_string(),
                            lane: lane_str.to_string(),
                            field: "rrf_constant".to_string(),
                            message: format!("rrf_constant mismatch: running={r:?}, pinned={p:?}"),
                        });
                    }
                }

                // Check plan_order_index equality
                if running_entry.plan_order_index != pinned_entry.plan_order_index {
                    return Err(PlanTableError {
                        shape: shape_str.to_string(),
                        lane: lane_str.to_string(),
                        field: "plan_order_index".to_string(),
                        message: format!(
                            "plan_order_index mismatch: running={}, pinned={}",
                            running_entry.plan_order_index, pinned_entry.plan_order_index
                        ),
                    });
                }
            }

            // Check for extra lanes in pinned table
            for pinned_lane in pinned_lanes.keys() {
                if !running_lanes.contains_key(pinned_lane) {
                    return Err(PlanTableError {
                        shape: shape_str.to_string(),
                        lane: pinned_lane.clone(),
                        field: "pair".to_string(),
                        message: "extra lane in pinned table".to_string(),
                    });
                }
            }
        }

        // Check for extra shapes in pinned table
        for pinned_shape in pinned.entries.keys() {
            if !self.entries.contains_key(pinned_shape) {
                return Err(PlanTableError {
                    shape: pinned_shape.clone(),
                    lane: "all".to_string(),
                    field: "pair".to_string(),
                    message: "extra shape in pinned table".to_string(),
                });
            }
        }

        Ok(())
    }
}

/// Embedded pinned plan table constant compiled into the binary.
pub const PINNED_PLAN_TABLE_JSON: &str =
    include_str!("../../../../../benchmarks/aft-search/engine-fixtures/plan-table.json");

/// Load and verify the pinned plan table at startup against the running table.
pub fn verify_pinned_plan_table_at_startup() -> Result<(), PlanTableError> {
    let running = PlanTable::running_table();
    let pinned = PlanTable::from_json(PINNED_PLAN_TABLE_JSON)?;
    running.verify_against(&pinned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_table_matches_pinned_json() {
        verify_pinned_plan_table_at_startup()
            .expect("pinned plan-table.json must match running table");
    }

    #[test]
    #[ignore = "fixture regeneration is an explicit maintainer action"]
    fn regenerate_pinned_plan_table() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../benchmarks/aft-search/engine-fixtures/plan-table.json");
        let json = serde_json::to_string_pretty(&PlanTable::running_table())
            .expect("serialize running plan table");
        std::fs::write(fixture, format!("{json}\n")).expect("write pinned plan table");
    }

    #[test]
    fn detects_missing_pair() {
        let running = PlanTable::running_table();
        let mut pinned = PlanTable::running_table();
        pinned
            .entries
            .get_mut("identifier")
            .unwrap()
            .remove("lexical");

        let err = running.verify_against(&pinned).unwrap_err();
        assert_eq!(err.shape, "identifier");
        assert_eq!(err.lane, "lexical");
        assert_eq!(err.field, "pair");
    }

    #[test]
    fn detects_extra_pair() {
        let running = PlanTable::running_table();
        let mut pinned = PlanTable::running_table();
        pinned.entries.get_mut("identifier").unwrap().insert(
            "custom".to_string(),
            LanePlanEntry {
                weight: Some(0.5),
                rrf_constant: Some(60.0),
                plan_order_index: 3,
            },
        );

        let err = running.verify_against(&pinned).unwrap_err();
        assert_eq!(err.shape, "identifier");
        assert_eq!(err.lane, "custom");
        assert_eq!(err.field, "pair");
    }

    #[test]
    fn detects_weight_change() {
        let running = PlanTable::running_table();
        let mut pinned = PlanTable::running_table();
        pinned
            .entries
            .get_mut("identifier")
            .unwrap()
            .get_mut("lexical")
            .unwrap()
            .weight = Some(0.75);

        let err = running.verify_against(&pinned).unwrap_err();
        assert_eq!(err.shape, "identifier");
        assert_eq!(err.lane, "lexical");
        assert_eq!(err.field, "weight");
    }

    #[test]
    fn detects_all_shape_rrf_change() {
        let running = PlanTable::running_table();
        let mut pinned = PlanTable::running_table();
        for (_, lanes) in pinned.entries.iter_mut() {
            for (lane_name, entry) in lanes.iter_mut() {
                if lane_name != "exact" {
                    entry.rrf_constant = Some(50.0);
                }
            }
        }

        let err = running.verify_against(&pinned).unwrap_err();
        assert_eq!(err.field, "rrf_constant");
    }

    #[test]
    fn detects_exact_lane_zero_weight() {
        let running = PlanTable::running_table();
        let mut pinned = PlanTable::running_table();
        pinned
            .entries
            .get_mut("identifier")
            .unwrap()
            .get_mut("exact")
            .unwrap()
            .weight = Some(0.0);

        let err = running.verify_against(&pinned).unwrap_err();
        assert_eq!(err.shape, "identifier");
        assert_eq!(err.lane, "exact");
        assert_eq!(err.field, "weight");
    }
}
