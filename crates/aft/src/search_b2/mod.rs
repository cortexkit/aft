//! Home of the aft_search shape router, lane plans, token variants and
//! readiness sampling (campaign B2). Created by the ranking-engine
//! integration so B2's slices have a compilable module and one install point
//! from their first commit; ownership of everything under `search_b2/`
//! transfers to campaign B2 at its first slice.
//!
//! The engine never names B2's types: it asks this module for the
//! [`SearchExtensions`] implementation to run, and receives the A-side
//! defaults until B2 installs its own.

use crate::commands::semantic_search::extensions::{DefaultSearchExtensions, SearchExtensions};

pub mod embed_counter;
pub mod readiness;

static DEFAULTS: DefaultSearchExtensions = DefaultSearchExtensions;

/// The [`SearchExtensions`] the live search path runs. Returns the A-side
/// defaults until B2's slices replace the installation.
pub fn install_defaults() -> &'static dyn SearchExtensions {
    &DEFAULTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::semantic_search::extensions::{classify_query_facts, QueryFacts};

    #[test]
    fn defaults_classify_like_the_engine() {
        let facts = QueryFacts::new("fn parse_manifest");
        assert_eq!(
            install_defaults().classify(&facts),
            classify_query_facts(&facts)
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
