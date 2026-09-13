//! Wall-time attribution for view materialization.

use std::cell::RefCell;
use std::time::Instant;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhaseTimings {
    pub(crate) load_bindings_select_ms: u128,
    pub(crate) delete_rows_ms: u128,
    pub(crate) owned_blob_decode_and_insert_ms: u128,
    pub(crate) join_load_payloads_ms: u128,
    pub(crate) join_decode_bind_index_entries_ms: u128,
    pub(crate) join_index_and_surface_replay_ms: u128,
    pub(crate) join_decode_resolved_callers_ms: u128,
    pub(crate) join_resolve_and_record_ms: u128,
    pub(crate) join_dependency_union_ms: u128,
    pub(crate) selected_join_ms: u128,
    pub(crate) write_bindings_ms: u128,
    pub(crate) emit_refs_edges_ms: u128,
    pub(crate) commit_ms: u128,
}

thread_local! {
    static ACTIVE_MATERIALIZATION: RefCell<Option<PhaseTimings>> = const { RefCell::new(None) };
}

pub(crate) struct PhaseTimer {
    start: Instant,
    emit_stderr: bool,
    prefix: &'static str,
}

impl PhaseTimer {
    pub(crate) fn new(prefix: &'static str) -> Self {
        if prefix != "join" {
            ACTIVE_MATERIALIZATION.with(|active| active.replace(Some(PhaseTimings::default())));
        }
        Self {
            start: Instant::now(),
            emit_stderr: std::env::var_os("AFT_VIEW_PROFILE").is_some(),
            prefix,
        }
    }

    pub(crate) fn finish(&mut self, phase: &str) {
        let elapsed = self.start.elapsed();
        let elapsed_ms = elapsed.as_millis();
        ACTIVE_MATERIALIZATION.with(|active| {
            let mut active = active.borrow_mut();
            let Some(timings) = active.as_mut() else {
                return;
            };
            match (self.prefix, phase) {
                (_, "load_bindings_select") => timings.load_bindings_select_ms = elapsed_ms,
                (_, "delete_rows") => timings.delete_rows_ms = elapsed_ms,
                (_, "owned_blob_decode_and_insert") => {
                    timings.owned_blob_decode_and_insert_ms = elapsed_ms;
                }
                ("join", "load_payloads") => timings.join_load_payloads_ms = elapsed_ms,
                ("join", "decode_bind_index_entries") => {
                    timings.join_decode_bind_index_entries_ms = elapsed_ms;
                }
                ("join", "index_and_surface_replay") => {
                    timings.join_index_and_surface_replay_ms = elapsed_ms;
                }
                ("join", "decode_resolved_callers") => {
                    timings.join_decode_resolved_callers_ms = elapsed_ms;
                }
                ("join", "resolve_and_record") => {
                    timings.join_resolve_and_record_ms = elapsed_ms;
                }
                ("join", "dependency_union") => {
                    timings.join_dependency_union_ms = elapsed_ms;
                }
                (_, "selected_join") => timings.selected_join_ms = elapsed_ms,
                (_, "write_bindings") => timings.write_bindings_ms = elapsed_ms,
                (_, "emit_refs_edges") => timings.emit_refs_edges_ms = elapsed_ms,
                (_, "commit") => timings.commit_ms = elapsed_ms,
                _ => {}
            }
        });
        if self.emit_stderr {
            eprintln!(
                "view_profile {}.{phase} ms={:.3}",
                self.prefix,
                elapsed.as_secs_f64() * 1000.0
            );
        }
        self.start = Instant::now();
    }

    pub(crate) fn into_timings(self) -> PhaseTimings {
        ACTIVE_MATERIALIZATION
            .with(|active| active.borrow_mut().take())
            .unwrap_or_default()
    }
}
