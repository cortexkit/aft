// The explicit crate-private imports preserve the old visibility of engine
// implementation modules, overriding the facade's public glob.
#![allow(hidden_glob_reexports)]

pub use aft_engine::data_layer::*;
pub(crate) use aft_engine::data_layer::{
    grammar, graph_store, meaning_query, outline, parent, query, runtime, text_scope,
};

pub mod trace_measure {
    pub use crate::trace_measure_run::run;
    pub use aft_engine::data_layer::trace_measure::*;
}
