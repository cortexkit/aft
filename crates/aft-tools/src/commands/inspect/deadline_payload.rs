use std::sync::OnceLock;

use super::*;

const WAIT_BUDGET_REASON: &str = "analysis did not finish within its wait budget";

#[derive(Clone, Copy)]
pub(super) enum ScopeCompleteness {
    Complete,
    WalkTruncated,
    DirectoryCountPending,
}

impl From<bool> for ScopeCompleteness {
    fn from(complete: bool) -> Self {
        if complete {
            Self::Complete
        } else {
            Self::WalkTruncated
        }
    }
}

impl ScopeCompleteness {
    pub(super) fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }

    pub(super) fn gap(self) -> Option<Value> {
        match self {
            Self::Complete => None,
            Self::WalkTruncated => Some(scope_walk_gap()),
            Self::DirectoryCountPending => Some(serde_json::json!({
                "kind": "scope_inventory_pending",
                "reason": "directory files not yet counted",
            })),
        }
    }

    pub(super) fn count_suffix(self, count: u64) -> &'static str {
        match self {
            Self::Complete if count == 0 => " (no analyzed files under this scope)",
            Self::WalkTruncated => " (file walk incomplete)",
            _ => "",
        }
    }
}

pub(super) type ScopeInventory<'a> = (&'a [PathBuf], u64, ScopeCompleteness);

pub(super) struct InspectTimeoutPayload {
    initial: Value,
    inventoried: OnceLock<Value>,
}

impl InspectTimeoutPayload {
    pub(super) fn new(
        req: &RawRequest,
        ctx: &AppContext,
        snapshot: &InspectSnapshot,
        scope: &ParsedScope,
    ) -> Self {
        let files = scope
            .roots
            .iter()
            .filter(|path| path.is_file())
            .cloned()
            .collect::<Vec<_>>();
        let completeness = if files.len() == scope.roots.len() {
            ScopeCompleteness::Complete
        } else {
            ScopeCompleteness::DirectoryCountPending
        };
        let inventory = (!scope.roots.is_empty()).then_some((
            scope.roots.as_slice(),
            files.len() as u64,
            completeness,
        ));
        Self {
            initial: deadline_payload(req, ctx, snapshot, inventory, &files, None),
            inventoried: OnceLock::new(),
        }
    }

    // Render before waiting for diagnostics so the reply poll only reads
    // published data, even if the worker holds a lock needed to collect diagnostics.
    pub(super) fn publish_inventory(
        &self,
        req: &RawRequest,
        ctx: &AppContext,
        snapshot: &InspectSnapshot,
        inventory: ScopeInventory<'_>,
        files: &[PathBuf],
    ) {
        let _ = self.inventoried.set(deadline_payload(
            req,
            ctx,
            snapshot,
            Some(inventory),
            files,
            Some(&self.initial["scanner_state"]),
        ));
    }

    pub(super) fn get(&self) -> Value {
        self.inventoried.get().unwrap_or(&self.initial).clone()
    }
}

fn deadline_payload(
    req: &RawRequest,
    ctx: &AppContext,
    snapshot: &InspectSnapshot,
    inventory: Option<ScopeInventory<'_>>,
    files: &[PathBuf],
    saved_scanner_state: Option<&Value>,
) -> Value {
    let sections = parse_sections(req.params.get("sections")).expect("validated sections");
    let scoped = inventory.is_some();
    let mut payloads = BTreeMap::new();
    let mut category_reports_timeout = false;
    for category in InspectCategory::active().iter().copied() {
        let requested = !scoped
            || if category == InspectCategory::Diagnostics {
                sections.requests_scoped_diagnostics()
            } else {
                sections.detail_categories.is_empty() || sections.includes(category)
            };
        category_reports_timeout |= requested && snapshot.config.inspect.category_enabled(category);
        let gap = serde_json::json!({"kind": "analysis_incomplete", "reason": WAIT_BUDGET_REASON});
        let payload = if !requested {
            serde_json::json!({"status": "not_requested"})
        } else if category == InspectCategory::Diagnostics {
            let mut gaps = files.iter().map(|file| serde_json::json!({
                "kind": "uncovered_file", "file": scope_root_display(&snapshot.project_root, file),
                "reason": crate::inspect::diagnostics_category::CURRENT_CONTENT_UNVERIFIED_REASON,
            })).collect::<Vec<_>>();
            gaps.push(gap);
            serde_json::json!({"complete": false, "items": [], "gaps": gaps})
        } else {
            serde_json::json!({"unavailable": true, "complete": false, "gaps": [gap]})
        };
        payloads.insert(category, payload);
    }
    let mut payload = build_inspect_payload_with_offset(
        snapshot,
        &payloads,
        &sections,
        parse_top_k(&req.params).expect("validated top_k"),
        parse_offset(&req.params).expect("validated offset"),
        ctx,
        inventory,
        saved_scanner_state,
    );
    if !category_reports_timeout {
        payload
            .as_object_mut()
            .unwrap()
            .entry("gaps")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "kind": "analysis_incomplete", "reason": WAIT_BUDGET_REASON,
            }));
    }
    payload
}
