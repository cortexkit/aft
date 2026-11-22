// Keep the public analysis modules at aft::data_layer; engine implementation
// modules used only by command handlers remain crate-private below.
#[doc(hidden)]
pub use aft_engine::data_layer::read_fence;
#[cfg(feature = "test-hooks")]
pub use aft_engine::data_layer::refresh_probe;
pub use aft_engine::data_layer::{
    callgraph, callgraph_roots, callgraph_store_adapter, checkout, checkout_gate, content,
    dead_code_graph, dead_code_snapshot, edit_gate, extract_gate, fragment, host, incremental,
    old_index, parent_session, reuse, standing, status, surfaces, symbol_prewarm_measure, vectors,
};
pub(crate) use aft_engine::data_layer::{
    grammar, graph_store, meaning_query, outline, parent, query, runtime, text_scope,
};

pub mod trace_measure {
    pub use crate::trace_measure_run::run;
    pub(crate) use aft_engine::data_layer::trace_measure::{build, Built};
}
