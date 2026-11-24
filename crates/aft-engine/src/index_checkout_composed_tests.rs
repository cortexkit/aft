use crate::data_layer::checkout::composed_checks;
use crate::inspect::InspectEntryPoints;

macro_rules! checkout_case {
    ($case:ident) => {
        #[test]
        fn $case() {
            composed_checks::$case(&InspectEntryPoints);
        }
    };
}

checkout_case!(memory_figure_read_side_allocations_advance_the_checkout_epoch);
checkout_case!(folding_published_changes_answers_as_before_and_as_a_cold_build);
checkout_case!(a_checkout_that_stopped_publishing_is_folded_on_its_behalf);
checkout_case!(an_edit_updates_the_call_graph_without_a_rebuild);
checkout_case!(a_built_checkout_answers_text_and_call_graph_queries_and_survives_a_restart);
