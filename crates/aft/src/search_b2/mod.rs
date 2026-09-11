//! Home of the aft_search shape router, lane plans, token variants and
//! readiness sampling (campaign B2). Created by the ranking-engine
//! integration so B2's slices have a compilable module and one install point
//! from their first commit; ownership of everything under `search_b2/`
//! transfers to campaign B2 at its first slice.
//!
//! The engine asks this module for one [`SearchExtensions`] implementation;
//! every B2 hook is composed here without making the engine name a concrete
//! router, planner, readiness sampler, variant pipeline, or counter.

pub mod embed_counter;
pub mod lane_plan;
pub mod readiness;
pub mod router;
pub mod variants;

use crate::commands::semantic_search::extensions::{
    LanePlan, QueryFacts, RawQuery, Readiness, SearchExtensions, Token, TokenVariant,
};
use crate::commands::semantic_search::plan_table::SearchShape;

#[derive(Debug, Default, Clone, Copy)]
struct B2SearchExtensions;

impl SearchExtensions for B2SearchExtensions {
    fn classify(&self, raw_query: &RawQuery) -> (SearchShape, QueryFacts) {
        router::classify(raw_query)
    }

    fn variants(&self, token: Token<'_>) -> Vec<TokenVariant> {
        variants::generate_variants(token)
    }

    fn plan<'a>(
        &self,
        shape: &SearchShape,
        facts: &QueryFacts,
        readiness: &Readiness<'a>,
    ) -> LanePlan<'a> {
        lane_plan::plan(shape, facts, readiness)
    }
}

static EXTENSIONS: B2SearchExtensions = B2SearchExtensions;

/// Returns the single extension set used by the live search path.
pub fn install_defaults() -> &'static dyn SearchExtensions {
    &EXTENSIONS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::semantic_search::extensions::{
        classify_raw_query, DefaultSearchExtensions,
    };

    #[test]
    fn default_classifier_remains_available_beside_the_b2_installation() {
        let raw_query = RawQuery::new("fn parse_manifest");
        assert_eq!(
            DefaultSearchExtensions.classify(&raw_query),
            classify_raw_query(&raw_query)
        );
        assert_eq!(
            install_defaults().classify(&raw_query).0,
            SearchShape::Short
        );
    }

    #[test]
    fn counter_is_reachable_through_the_bootstrap() {
        let request_id = "bootstrap-counter";
        let _guard = embed_counter::install(request_id);
        embed_counter::record(embed_counter::EmbedCounts {
            requested: 1,
            cache_hits: 0,
            live_calls: 0,
        });
        assert_eq!(embed_counter::read(request_id).requested, 1);
    }
}
