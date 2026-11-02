use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use parking_lot::Mutex;
use serde::Serialize;

use super::InspectCategory;
use crate::lsp::roots::ServerKey;

const RETAINED_CALLS: usize = 64;

/// The fixed vocabulary for inspect work. These IDs describe work completed by
/// the server; they are not transport or dispatch milestones.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InspectPhaseId {
    LspStart,
    LspQuiescence,
    Tier2Rescan,
    CallgraphReady,
    StatVerification,
}

impl InspectPhaseId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LspStart => "lsp_start",
            Self::LspQuiescence => "lsp_quiescence",
            Self::Tier2Rescan => "tier2_rescan",
            Self::CallgraphReady => "callgraph_ready",
            Self::StatVerification => "stat_verification",
        }
    }

    const fn takes_producer(self) -> bool {
        matches!(self, Self::LspStart | Self::LspQuiescence)
    }

    const fn takes_category(self) -> bool {
        matches!(
            self,
            Self::Tier2Rescan | Self::CallgraphReady | Self::StatVerification
        )
    }
}

/// One completed phase's agent-facing logical shape. `also_satisfied` is absent
/// unless one physical work unit freshened more than its primary category.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct InspectPhaseEntry {
    pub id: InspectPhaseId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub also_satisfied: Option<Vec<String>>,
}

impl InspectPhaseEntry {
    pub fn lsp(id: InspectPhaseId, server: &ServerKey) -> Self {
        assert!(id.takes_producer(), "producer is only valid for LSP phases");
        Self {
            id,
            producer: Some(server.kind.id_str().to_string()),
            category: None,
            also_satisfied: None,
        }
    }

    pub fn category(id: InspectPhaseId, category: InspectCategory) -> Self {
        assert!(
            id.takes_category(),
            "category is only valid for category-attributed phases"
        );
        Self {
            id,
            producer: None,
            category: Some(category.as_str().to_string()),
            also_satisfied: None,
        }
    }

    pub fn with_also_satisfied(
        mut self,
        categories: impl IntoIterator<Item = InspectCategory>,
    ) -> Self {
        let mut seen = HashSet::new();
        let categories = categories
            .into_iter()
            .map(|category| category.as_str().to_string())
            .filter(|category| seen.insert(category.clone()))
            .collect::<Vec<_>>();
        if !categories.is_empty() {
            self.also_satisfied = Some(categories);
        }
        self
    }
}

#[derive(Clone, Debug)]
pub struct InspectPhaseRecord {
    pub entry: InspectPhaseEntry,
    pub started: Instant,
    pub completed: Option<Instant>,
    pub terminal_error: Option<String>,
}

impl InspectPhaseRecord {
    pub fn is_completed(&self) -> bool {
        self.completed.is_some()
    }

    pub fn terminal_error(&self) -> Option<&str> {
        self.terminal_error.as_deref()
    }

    pub fn duration_ms(&self) -> Option<u128> {
        self.completed
            .map(|completed| completed.duration_since(self.started).as_millis())
    }
}

#[derive(Clone, Debug)]
pub struct InspectPhaseLogSnapshot {
    pub request_id: String,
    pub records: Vec<InspectPhaseRecord>,
    pub blocking_waited: bool,
}

#[derive(Default)]
struct InspectPhaseLogState {
    request_id: String,
    records: Vec<InspectPhaseRecord>,
    blocking_waited: bool,
}

/// Server-side per-call inspect evidence. It is deliberately independent of
/// `run_tool_call::PhaseTrace`, whose records describe subc dispatch milestones
/// rather than inspection work.
#[derive(Clone, Default)]
pub struct InspectPhaseLog {
    state: Arc<Mutex<InspectPhaseLogState>>,
}

impl InspectPhaseLog {
    pub fn for_request(request_id: impl Into<String>) -> Self {
        let log = Self {
            state: Arc::new(Mutex::new(InspectPhaseLogState {
                request_id: request_id.into(),
                ..InspectPhaseLogState::default()
            })),
        };
        retain_log(log.clone());
        log
    }

    pub fn start(&self, entry: InspectPhaseEntry) -> InspectPhaseHandle {
        crate::run_tool_call::progress::report();
        let mut state = self.state.lock();
        let index = state.records.len();
        state.records.push(InspectPhaseRecord {
            entry,
            started: Instant::now(),
            completed: None,
            terminal_error: None,
        });
        InspectPhaseHandle {
            log: self.clone(),
            index,
            completed: false,
        }
    }

    pub fn note_blocking_wait(&self) {
        self.state.lock().blocking_waited = true;
    }

    pub fn snapshot(&self) -> InspectPhaseLogSnapshot {
        let state = self.state.lock();
        InspectPhaseLogSnapshot {
            request_id: state.request_id.clone(),
            records: state.records.clone(),
            blocking_waited: state.blocking_waited,
        }
    }

    /// Reads the two formatter inputs from the single per-call record set.
    pub fn terminal_inputs(&self) -> (Vec<InspectPhaseEntry>, bool) {
        let state = self.state.lock();
        let entries = state
            .records
            .iter()
            .filter(|record| record.is_completed() && record.terminal_error.is_none())
            .map(|record| record.entry.clone())
            .collect();
        (entries, state.blocking_waited)
    }

    /// The last begun, incomplete phase is the only phase shutdown may name.
    /// When work has not begun, callers render the preflight failure form.
    pub fn in_flight_entry(&self) -> Option<InspectPhaseEntry> {
        self.state
            .lock()
            .records
            .iter()
            .rev()
            .find(|record| !record.is_completed())
            .map(|record| record.entry.clone())
    }
}

pub struct InspectPhaseHandle {
    log: InspectPhaseLog,
    index: usize,
    completed: bool,
}

impl InspectPhaseHandle {
    pub fn complete(mut self) {
        crate::run_tool_call::progress::report();
        let mut state = self.log.state.lock();
        if let Some(record) = state.records.get_mut(self.index) {
            record.completed = Some(Instant::now());
        }
        self.completed = true;
    }

    pub fn fail(mut self, error: impl Into<String>) {
        crate::run_tool_call::progress::report();
        let mut state = self.log.state.lock();
        if let Some(record) = state.records.get_mut(self.index) {
            record.completed = Some(Instant::now());
            record.terminal_error = Some(error.into());
        }
        self.completed = true;
    }
}

impl Drop for InspectPhaseHandle {
    fn drop(&mut self) {
        if !self.completed {
            let mut state = self.log.state.lock();
            if let Some(record) = state.records.get_mut(self.index) {
                record.completed = Some(Instant::now());
                record.terminal_error = Some("phase handle dropped before completion".to_string());
            }
        }
    }
}

struct RetainedLogs {
    logs: BTreeMap<String, InspectPhaseLog>,
    order: VecDeque<String>,
}

static RETAINED_LOGS: LazyLock<Mutex<RetainedLogs>> = LazyLock::new(|| {
    Mutex::new(RetainedLogs {
        logs: BTreeMap::new(),
        order: VecDeque::new(),
    })
});

fn retain_log(log: InspectPhaseLog) {
    let request_id = log.snapshot().request_id;
    let mut retained = RETAINED_LOGS.lock();
    if !retained.logs.contains_key(&request_id) {
        retained.order.push_back(request_id.clone());
    }
    retained.logs.insert(request_id, log);
    while retained.order.len() > RETAINED_CALLS {
        if let Some(expired) = retained.order.pop_front() {
            retained.logs.remove(&expired);
        }
    }
}

/// Harness-facing accessor for the retained evidence associated with a request.
/// The bounded retention window covers delivery and post-terminal assertions.
pub fn inspect_phase_log_for_request(request_id: &str) -> Option<InspectPhaseLogSnapshot> {
    RETAINED_LOGS
        .lock()
        .logs
        .get(request_id)
        .map(InspectPhaseLog::snapshot)
}

/// The only formatter for the human wait text. Callers obtain both inputs from
/// [`InspectPhaseLog::terminal_inputs`] rather than reconstructing them.
///
/// Phases are counted, not listed: a project with two dozen language servers
/// completes `lsp_start` two dozen times, and naming each one buried the
/// line. Each phase appears once, in first-completed order, with its count
/// and a per-producer (or per-category) breakdown, largest first:
/// `lsp_start ×25 (typescript 12, bash 6, rust 4, python 2, biome 1)`.
pub fn format_wait_text(entries: &[InspectPhaseEntry], blocking_waited: bool) -> String {
    let completed = if entries.is_empty() {
        "none".to_string()
    } else {
        collapse_phase_entries(entries).join(", ")
    };
    format!(
        "waited: {}; completed: {completed}",
        if blocking_waited { "yes" } else { "no" }
    )
}

/// One summary per phase id: `id`, `id (label)` for a single attributed
/// entry, or `id ×N (label n, ...)` for several.
pub fn collapse_phase_entries(entries: &[InspectPhaseEntry]) -> Vec<String> {
    let mut phases: Vec<(InspectPhaseId, Vec<(String, usize)>, usize)> = Vec::new();
    for entry in entries {
        let index = match phases.iter().position(|(id, _, _)| *id == entry.id) {
            Some(index) => index,
            None => {
                phases.push((entry.id, Vec::new(), 0));
                phases.len() - 1
            }
        };
        let (_, labels, count) = &mut phases[index];
        *count += 1;
        if let Some(label) = entry.producer.as_ref().or(entry.category.as_ref()) {
            match labels.iter_mut().find(|(known, _)| known == label) {
                Some((_, n)) => *n += 1,
                None => labels.push((label.clone(), 1)),
            }
        }
    }
    phases
        .into_iter()
        .map(|(id, mut labels, count)| {
            // `sort_by` is stable, so labels with equal counts keep the
            // order in which their phases first completed.
            labels.sort_by(|left, right| right.1.cmp(&left.1));
            let id = id.as_str();
            match (count, labels.as_slice()) {
                (_, []) if count == 1 => id.to_string(),
                (_, []) => format!("{id} ×{count}"),
                (1, [(label, _)]) => format!("{id} ({label})"),
                _ => format!(
                    "{id} ×{count} ({})",
                    labels
                        .iter()
                        .map(|(label, n)| format!("{label} {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_text_reads_completed_phase_order_and_recorded_wait_state() {
        let log = InspectPhaseLog::for_request("inspect-log-order");
        log.start(InspectPhaseEntry::category(
            InspectPhaseId::StatVerification,
            InspectCategory::DeadCode,
        ))
        .complete();
        let (entries, waited) = log.terminal_inputs();
        assert_eq!(
            format_wait_text(&entries, waited),
            "waited: no; completed: stat_verification (dead_code)"
        );

        log.note_blocking_wait();
        let (entries, waited) = log.terminal_inputs();
        assert_eq!(
            format_wait_text(&entries, waited),
            "waited: yes; completed: stat_verification (dead_code)"
        );
    }

    fn lsp_entry(id: InspectPhaseId, producer: &str) -> InspectPhaseEntry {
        InspectPhaseEntry {
            id,
            producer: Some(producer.to_string()),
            category: None,
            also_satisfied: None,
        }
    }

    /// Repeated phases collapse to one count per phase with a per-producer
    /// breakdown, largest first, ties in first-seen order.
    #[test]
    fn wait_text_counts_repeated_phases_per_producer() {
        let mut entries = Vec::new();
        for (producer, count) in [("bash", 2), ("typescript", 3), ("rust", 1), ("biome", 1)] {
            for _ in 0..count {
                entries.push(lsp_entry(InspectPhaseId::LspStart, producer));
            }
        }
        entries.push(lsp_entry(InspectPhaseId::LspQuiescence, "rust"));
        entries.push(InspectPhaseEntry::category(
            InspectPhaseId::Tier2Rescan,
            InspectCategory::DeadCode,
        ));
        entries.push(InspectPhaseEntry::category(
            InspectPhaseId::Tier2Rescan,
            InspectCategory::Duplicates,
        ));
        assert_eq!(
            format_wait_text(&entries, true),
            "waited: yes; completed: lsp_start ×7 (typescript 3, bash 2, rust 1, biome 1), \
             lsp_quiescence (rust), tier2_rescan ×2 (dead_code 1, duplicates 1)"
        );
    }

    #[test]
    fn failed_records_do_not_become_completed_phase_entries() {
        let log = InspectPhaseLog::for_request("inspect-log-incomplete");
        let phase = log.start(InspectPhaseEntry::category(
            InspectPhaseId::Tier2Rescan,
            InspectCategory::Duplicates,
        ));
        phase.fail("scan failed");

        let (entries, waited) = log.terminal_inputs();
        assert!(entries.is_empty());
        assert!(!waited);
        let snapshot = inspect_phase_log_for_request("inspect-log-incomplete").unwrap();
        assert_eq!(snapshot.records[0].terminal_error(), Some("scan failed"));
        assert!(snapshot.records[0].completed.is_some());
        assert!(snapshot.records[0].duration_ms().is_some());
    }
}
