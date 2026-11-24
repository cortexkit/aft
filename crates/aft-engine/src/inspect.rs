pub use aft_inspect::inspect::*;

#[doc(hidden)]
pub mod diagnostics_category {
    use std::time::Instant;

    pub use aft_inspect::inspect::diagnostics_category::*;

    use crate::context::AppContext;
    use crate::inspect::{InspectSnapshot, JobOutcome, JobScope};
    use crate::lsp::manager::{ApplicableServerFailure, NotApplicableServer};
    use crate::lsp::roots::ServerKey;

    pub fn start_inspect_rust_checks(
        ctx: &AppContext,
        producers: &[ServerKey],
        deadline: Instant,
    ) -> Vec<InspectRustCheck> {
        aft_inspect::inspect::diagnostics_category::start_inspect_rust_checks(
            ctx, producers, deadline,
        )
    }

    pub fn run_diagnostics_category(
        ctx: &AppContext,
        snapshot: &InspectSnapshot,
        scope: &JobScope,
        scope_was_provided: bool,
        scope_candidates: Option<&ScopedCoverageCandidates>,
        applicability_is_empty: bool,
        producer_failures: &[ApplicableServerFailure],
        not_applicable: &[NotApplicableServer],
        expected_producers: &[ServerKey],
        indexing_gaps: &[(ServerKey, String)],
        sweep_deadline: Option<Instant>,
        rust_checks: Vec<InspectRustCheck>,
    ) -> JobOutcome {
        aft_inspect::inspect::diagnostics_category::run_diagnostics_category(
            ctx,
            snapshot,
            scope,
            scope_was_provided,
            scope_candidates,
            applicability_is_empty,
            producer_failures,
            not_applicable,
            expected_producers,
            indexing_gaps,
            sweep_deadline,
            rust_checks,
        )
    }
}
