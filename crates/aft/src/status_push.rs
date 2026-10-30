//! Root-local status push projection; explicit status reads keep their full census.
use crate::context::AppContext;
use serde_json::Value;

const REFUSAL_DETAILS: usize = 8;
pub(crate) const BODY_MAX_BYTES: usize = 16 * 1024;
// Preserve every fixed InspectCategory name, with an explicit overflow signal.
const COMPACTED_DISABLED_CATEGORIES: usize = 13;
// Keep a bounded sample of degradation reasons; status_push_list_omitted reports the rest.
const COMPACTED_DEGRADED_REASONS: usize = REFUSAL_DETAILS;
// A handful of producer gaps fits the push budget; the remainder is counted.
const COMPACTED_DIAGNOSTIC_GAPS: usize = 8;
// Bound free-form text nested in these otherwise small lists.
const COMPACTED_STATUS_TEXT_BYTES: usize = 128;

pub(crate) struct StatusBody {
    pub bytes: Vec<u8>,
    pub original_bytes: usize,
    pub field_bytes: std::collections::BTreeMap<String, usize>,
    pub compacted: bool,
}

/// Measure the whole wire body. The bounded fallback keeps the consumer's
/// scalar state and reports omitted detail instead of silently retaining stale state.
pub(crate) fn bounded_body(frame: &crate::protocol::StatusChangedFrame) -> StatusBody {
    let original = serde_json::to_vec(frame).expect("JSON status frame");
    let original_bytes = original.len();
    let field_bytes = frame
        .snapshot
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            (
                key.clone(),
                serde_json::to_vec(value).expect("JSON status field").len(),
            )
        })
        .collect();
    if original_bytes <= BODY_MAX_BYTES {
        return StatusBody {
            bytes: original,
            original_bytes,
            field_bytes,
            compacted: false,
        };
    }
    let mut bytes = Vec::new();
    for string_limit in [256, 64, 0] {
        let mut snapshot = summary(&frame.snapshot, string_limit);
        snapshot["push_details"] = serde_json::json!({"omitted":true,"read":"status","reason":"serialized_body_limit","original_body_bytes":original_bytes});
        let session = frame
            .session_id
            .as_ref()
            .filter(|session| session.len() <= string_limit)
            .cloned();
        let compact = crate::protocol::StatusChangedFrame::new(session, snapshot);
        bytes = serde_json::to_vec(&compact).expect("JSON status summary");
        if bytes.len() <= BODY_MAX_BYTES {
            break;
        }
    }
    // The fixed set of keys and scalar values, with no input-sized collections
    // or strings in the final pass, fits independently of the original payload.
    assert!(
        bytes.len() <= BODY_MAX_BYTES,
        "fixed status summary exceeded its wire budget"
    );
    StatusBody {
        bytes,
        original_bytes,
        field_bytes,
        compacted: true,
    }
}

fn scalar(value: &Value, string_limit: usize) -> Value {
    match value {
        Value::String(text) if text.len() > string_limit => {
            serde_json::json!("Details omitted; run status")
        }
        Value::Array(_) => serde_json::json!([]),
        Value::Object(_) => serde_json::json!({}),
        _ => value.clone(),
    }
}

fn compact_named_list(value: &Value, max_items: usize, max_text_bytes: usize) -> (Value, usize) {
    let Some(values) = value.as_array() else {
        return (value.clone(), 0);
    };
    let compacted = values
        .iter()
        .take(max_items)
        .map(|value| match value.as_str() {
            Some(text) if text.len() <= max_text_bytes => value.clone(),
            Some(_) => serde_json::json!("Details omitted; run status"),
            None => scalar(value, 0),
        })
        .collect();
    (
        Value::Array(compacted),
        values.len().saturating_sub(max_items),
    )
}

fn compact_diagnostic_gaps(value: &Value) -> (Value, usize) {
    let Some(values) = value.as_array() else {
        return (value.clone(), 0);
    };
    let compacted = values
        .iter()
        .take(COMPACTED_DIAGNOSTIC_GAPS)
        .map(|gap| {
            let mut fields = serde_json::Map::new();
            for field in ["language", "producer", "root"] {
                if let Some(value) = gap.get(field) {
                    let value = match value.as_str() {
                        Some(text) if text.len() > COMPACTED_STATUS_TEXT_BYTES => {
                            serde_json::json!("Details omitted; run status")
                        }
                        Some(_) => value.clone(),
                        None => scalar(value, 0),
                    };
                    fields.insert(field.into(), value);
                }
            }
            Value::Object(fields)
        })
        .collect();
    (
        Value::Array(compacted),
        values.len().saturating_sub(COMPACTED_DIAGNOSTIC_GAPS),
    )
}

fn summary(source: &Value, string_limit: usize) -> Value {
    let mut out = serde_json::Map::new();
    let mut lists = serde_json::Map::new();
    let mut omitted_lists = serde_json::Map::new();
    for key in [
        "version",
        "project_root",
        "canonical_root",
        "cache_role",
        "degraded",
        "degraded_reasons",
        "message",
        "config_error",
        "lsp_servers",
        "storage_dir",
        "checkpoints_total",
    ] {
        if let Some(value) = source.get(key) {
            if key == "degraded_reasons" {
                if let Some(values) = value.as_array() {
                    lists.insert(key.into(), serde_json::json!(values.len()));
                }
                let (compacted, omitted) = compact_named_list(
                    value,
                    COMPACTED_DEGRADED_REASONS,
                    COMPACTED_STATUS_TEXT_BYTES,
                );
                if omitted > 0 {
                    omitted_lists.insert(key.into(), serde_json::json!(omitted));
                }
                out.insert(key.into(), compacted);
            } else {
                out.insert(key.into(), scalar(value, string_limit));
            }
        }
    }
    // Keep fields used by plugin status parsing and progress displays. Pushes
    // retain bounded samples of user-visible lists; use a full status read for omitted items.
    let containers: &[(&str, &[&str])] = &[
        (
            "features",
            &[
                "format_on_edit",
                "validate_on_edit",
                "restrict_to_project_root",
                "search_index",
                "experimental_search_index",
                "semantic_search",
                "experimental_semantic_search",
                "callgraph_store",
            ],
        ),
        ("search_index", &["status", "files", "trigrams"]),
        (
            "semantic_index",
            &[
                "status",
                "backend",
                "model",
                "stage",
                "files",
                "entries_done",
                "entries_total",
                "refreshing_count",
                "entries",
                "dimension",
                "error",
                "reason",
                "backend_url",
                "pending_reason",
            ],
        ),
        ("semantic", &["backend", "model"]),
        ("semantic_config", &["backend", "model"]),
        (
            "disk",
            &["storage_dir", "trigram_disk_bytes", "semantic_disk_bytes"],
        ),
        (
            "runtime",
            &["live_watchers", "live_actor_roots", "open_routes"],
        ),
        ("symbol_cache", &["local_entries", "warm_entries"]),
        (
            "status_bar",
            &[
                "disabled_categories",
                "errors",
                "warnings",
                "dead_code",
                "unused_exports",
                "duplicates",
                "todos",
                "diagnostics",
                "diagnostics_gaps",
                "tier2_stale",
            ],
        ),
        (
            "status_bar_values",
            &[
                "disabled_categories",
                "errors",
                "warnings",
                "dead_code",
                "unused_exports",
                "duplicates",
                "todos",
                "diagnostics",
                "diagnostics_gaps",
                "tier2_stale",
            ],
        ),
    ];
    for (container, keys) in containers {
        if let Some(value) = source.get(*container) {
            if value.is_null() {
                out.insert((*container).into(), Value::Null);
                continue;
            }
            let mut selected = serde_json::Map::new();
            for key in *keys {
                if let Some(value) = value.get(*key) {
                    if let Some(rows) = value.as_array() {
                        lists.insert(format!("{container}.{key}"), serde_json::json!(rows.len()));
                    }
                    let (compacted, omitted) = match *key {
                        "disabled_categories" => compact_named_list(
                            value,
                            COMPACTED_DISABLED_CATEGORIES,
                            COMPACTED_STATUS_TEXT_BYTES,
                        ),
                        "diagnostics_gaps" => compact_diagnostic_gaps(value),
                        _ => (scalar(value, string_limit), 0),
                    };
                    if omitted > 0 {
                        omitted_lists
                            .insert(format!("{container}.{key}"), serde_json::json!(omitted));
                    }
                    selected.insert((*key).into(), compacted);
                }
            }
            out.insert((*container).into(), Value::Object(selected));
        }
    }
    if let Some(semantic) = out.get_mut("semantic_index").and_then(Value::as_object_mut) {
        for (field, keys) in [
            (
                "coordination_error",
                &[
                    "state",
                    "kind",
                    "reason",
                    "detail",
                    "retryable",
                    "retry_scheduled",
                    "retry_in_ms",
                ][..],
            ),
            ("pending_owner", &["kind", "pid", "attempt"][..]),
        ] {
            if let Some(value) = source.pointer(&format!("/semantic_index/{field}")) {
                let selected = keys
                    .iter()
                    .filter_map(|key| {
                        value
                            .get(*key)
                            .map(|value| ((*key).to_owned(), scalar(value, string_limit)))
                    })
                    .collect();
                semantic.insert(field.to_owned(), Value::Object(selected));
            }
        }
    }
    let mut compression = serde_json::Map::new();
    if let Some(project) = source.pointer("/compression/project") {
        let mut values = serde_json::Map::new();
        for key in [
            "events",
            "original_tokens",
            "compressed_tokens",
            "savings_tokens",
        ] {
            if let Some(value) = project.get(key) {
                values.insert(key.into(), scalar(value, string_limit));
            }
        }
        compression.insert("project".into(), Value::Object(values));
    }
    out.insert("compression".into(), Value::Object(compression));
    if let Some(reasons) = source.get("degraded_reasons").and_then(Value::as_array) {
        lists.insert("degraded_reasons".into(), serde_json::json!(reasons.len()));
    }
    out.insert("status_push_list_totals".into(), Value::Object(lists));
    if !omitted_lists.is_empty() {
        out.insert(
            "status_push_list_omitted".into(),
            Value::Object(omitted_lists),
        );
    }
    let total = source
        .get("storage_refusals_total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            source
                .get("storage_refusals")
                .and_then(Value::as_array)
                .map_or(0, |rows| rows.len() as u64)
        });
    out.insert("storage_refusals".into(), serde_json::json!([]));
    out.insert("storage_refusals_total".into(), serde_json::json!(total));
    out.insert("storage_refusals_omitted".into(), serde_json::json!(total));
    out.insert(
        "storage_refusals_unattributed".into(),
        source
            .get("storage_refusals_unattributed")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .into(),
    );
    let roots = source
        .pointer("/lsp/applicability_by_root")
        .and_then(Value::as_object);
    let mut counters = serde_json::Map::new();
    for key in [
        "hits",
        "misses",
        "coalesced_hits",
        "retained_content_batches",
        "warm_validations",
        "warm_validation_micros",
        "warm_validation_dependency_count",
        "dependency_count",
        "full_walks",
        "full_walk_micros",
    ] {
        let total = roots
            .into_iter()
            .flat_map(|roots| roots.values())
            .filter_map(|root| root.get(key).and_then(Value::as_u64))
            .fold(0u64, u64::saturating_add);
        counters.insert(key.into(), total.into());
    }
    let invalidations = roots
        .into_iter()
        .flat_map(|roots| roots.values())
        .filter_map(|root| root.get("invalidations").and_then(Value::as_object))
        .flat_map(|causes| causes.values())
        .filter_map(Value::as_u64)
        .fold(0u64, u64::saturating_add);
    counters.insert("invalidations".into(), invalidations.into());
    out.insert("lsp".into(),serde_json::json!({"applicability_roots_total":roots.map_or(0,|roots|roots.len()),"applicability_totals":counters}));
    Value::Object(out)
}

pub(crate) fn project(ctx: &AppContext, mut snapshot: Value) -> Value {
    let Some(object) = snapshot.as_object_mut() else {
        return snapshot;
    };
    // Process-wide scalars retain their meaning; cross-root detail lists never enter a push.
    object.retain(|key, _| {
        matches!(
            key.as_str(),
            "version"
                | "project_root"
                | "canonical_root"
                | "cache_role"
                | "degraded"
                | "degraded_reasons"
                | "message"
                | "config_error"
                | "features"
                | "search_index"
                | "semantic_index"
                | "semantic"
                | "semantic_config"
                | "disk"
                | "symbol_cache"
                | "runtime"
                | "lsp_servers"
                | "storage_dir"
                | "checkpoints_total"
                | "compression"
                | "status_bar"
                | "status_bar_values"
                | "session"
                | "views"
                | "watcher"
                | "artifact_owner"
                | "callgraph_refresh"
                | "lsp"
        )
    });
    if let Some(runtime) = object.get_mut("runtime").and_then(Value::as_object_mut) {
        runtime.retain(|key, _| {
            matches!(
                key.as_str(),
                "live_watchers" | "live_actor_roots" | "open_routes"
            )
        });
    }
    let config = ctx.config();
    let root = config.project_root.clone().unwrap_or_default();
    let normalized = crate::inspect::job::canonicalize_normalized(&root)
        .display()
        .to_string();
    let own = object
        .get("lsp")
        .and_then(|lsp| lsp["applicability_by_root"].get(&normalized))
        .cloned();
    let mut roots = serde_json::Map::new();
    if let Some(own) = own {
        roots.insert(normalized, own);
    }
    object.insert(
        "lsp".into(),
        serde_json::json!({"applicability_by_root":roots}),
    );

    let storage = crate::bash_background::storage_dir(config.storage_dir.as_deref());
    let canonical = ctx
        .canonical_cache_root_opt()
        .unwrap_or_else(|| root.clone());
    let key = ctx.cached_artifact_cache_key(&canonical);
    let (owned, unattributed) =
        crate::persisted_format::refusals_for_push(&storage, key.as_deref());
    let total = owned.len();
    let mut reasons: Vec<Value> = object
        .get("degraded_reasons")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|reason| {
            !reason
                .as_str()
                .is_some_and(|reason| reason.starts_with(crate::persisted_format::CODE))
        })
        .cloned()
        .collect();
    for refusal in &owned {
        if refusal.store == crate::persisted_format::PersistedStore::BashTask
            && refusal.source == crate::persisted_format::RefusalSource::Artifact
        {
            continue;
        }
        let reason = serde_json::json!(refusal.reason());
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    object.insert("degraded".into(), serde_json::json!(!reasons.is_empty()));
    object.insert("degraded_reasons".into(), Value::Array(reasons));
    let details: Vec<_> = owned
        .iter()
        .take(REFUSAL_DETAILS)
        .map(|refusal| {
            serde_json::json!({
                "code":crate::persisted_format::CODE,"store":refusal.store.name(),
                "path":refusal.path.display().to_string(),"found":refusal.found,
                "supported":refusal.supported,"message":refusal.to_string(),
            })
        })
        .collect();
    let omitted = total - details.len();
    object.insert("storage_refusals".into(), Value::Array(details));
    object.insert("storage_refusals_total".into(), serde_json::json!(total));
    object.insert(
        "storage_refusals_omitted".into(),
        serde_json::json!(omitted),
    );
    object.insert(
        "storage_refusals_unattributed".into(),
        serde_json::json!(unattributed),
    );
    object.insert(
        "push_details".into(),
        serde_json::json!({"omitted":omitted>0 || unattributed>0,"read":"status"}),
    );
    snapshot
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn semantic_batch_lease_operator_line_survives_compact_status_push() {
        for semantic in [
            serde_json::json!({
                "status": "ready",
                "pending_reason": "Waiting for another AFT process that is embedding the same new code; results will follow.",
                "pending_owner": {"kind":"base_claim", "pid":42, "attempt":3},
            }),
            serde_json::json!({
                "status": "ready",
                "pending_reason": "Can't coordinate embedding with other AFT processes: permission denied. Retrying in 4s.",
                "coordination_error": {
                    "state":"error", "kind":"storage", "reason":"Can't coordinate embedding with other AFT processes",
                    "detail":"permission denied", "retryable":true, "retry_scheduled":true, "retry_in_ms":4000,
                },
            }),
        ] {
            let frame = StatusChangedFrame::new(
                None,
                serde_json::json!({
                    "semantic_index": semantic.clone(),
                    "oversized_detail": "x".repeat(BODY_MAX_BYTES + 1),
                }),
            );
            let body = bounded_body(&frame);
            assert!(body.compacted);
            let captured: Value = serde_json::from_slice(&body.bytes).unwrap();
            assert_eq!(captured["snapshot"]["semantic_index"], semantic);
        }
    }

    use super::*;
    use crate::bash_background::push_projection_fixture::busy_root;
    use crate::persisted_format::{self, PersistedStore, UnsupportedPersistedFormat};
    use crate::protocol::StatusChangedFrame;
    use std::path::PathBuf;

    #[test]
    fn status_push_busy_projection_is_root_local_and_small() {
        let fixture = busy_root();
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(fixture.contexts.len(), 192);
        assert_eq!(fixture.app.open_route_count(), 90);
        assert_eq!(
            full["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            192
        );
        let frame = StatusChangedFrame::new(None, project(&fixture.ctx, full));
        let body = serde_json::to_vec(&frame).unwrap();
        assert!(
            body.len() < 8 * 1024,
            "busy projected body must be comfortably below 16 KiB, got {}",
            body.len()
        );
        let wire = body.len() + subc_protocol::HEADER_LEN;
        assert!(wire * 45 <= 1024 * 1024 && wire * 90 <= 1024 * 1024);
        eprintln!(
            "busy_status_body_bytes={} wire_45={} wire_90={}",
            body.len(),
            wire * 45,
            wire * 90
        );
        let roots = frame.snapshot["lsp"]["applicability_by_root"]
            .as_object()
            .unwrap();
        assert_eq!(roots.len(), 1);
        // The census is keyed by the resolved root (on macOS /var resolves to
        // /private/var), so compare against that form.
        let resolved_root = std::fs::canonicalize(&fixture.root).unwrap();
        assert!(
            roots.contains_key(&resolved_root.display().to_string()),
            "{roots:?}"
        );
        for field in ["memory", "sqlite", "threads", "children", "fds"] {
            assert!(
                frame.snapshot.get(field).is_none(),
                "process census {field} must remain in explicit reads only"
            );
        }
    }

    #[test]
    fn status_push_emitter_uses_root_local_projection() {
        let fixture = busy_root();
        let (tx, rx) = std::sync::mpsc::channel::<crate::protocol::PushFrame>();
        fixture
            .ctx
            .set_progress_sender(Some(std::sync::Arc::new(Box::new(move |frame| {
                let _ = tx.send(frame);
            }))));
        struct Reset<'a>(&'a AppContext);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set_progress_sender(None);
            }
        }
        let _reset = Reset(&fixture.ctx);
        for ready_snapshot in [false, true] {
            if ready_snapshot {
                fixture
                    .ctx
                    .status_emitter()
                    .signal(fixture.ctx.build_status_snapshot());
            } else {
                fixture.ctx.signal_status_changed();
            }
            let frame = loop {
                let frame = rx
                    .recv_timeout(std::time::Duration::from_secs(30))
                    .expect("status emitter produced a frame");
                if let crate::protocol::PushFrame::StatusChanged(frame) = frame {
                    break frame;
                }
            };
            assert_eq!(
                frame.snapshot["lsp"]["applicability_by_root"]
                    .as_object()
                    .unwrap()
                    .len(),
                1,
                "both emitter paths must use the projection"
            );
            assert!(frame.snapshot.get("memory").is_none());
            assert!(serde_json::to_vec(&frame).unwrap().len() < 8 * 1024);
        }
    }

    #[test]
    fn status_push_does_not_change_full_explicit_status_read() {
        let fixture = busy_root();
        let first = fixture.ctx.build_status_snapshot();
        let pushed = project(&fixture.ctx, first.clone());
        let after = fixture.ctx.build_status_snapshot();
        assert_eq!(
            after["lsp"]["applicability_by_root"],
            first["lsp"]["applicability_by_root"]
        );
        assert_eq!(
            after["memory"]["roots_total"],
            first["memory"]["roots_total"]
        );
        assert_eq!(after["memory"]["roots"].as_object().unwrap().len(), 8);
        assert_eq!(
            pushed["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            1,
            "explicit census must not leak into push"
        );
        assert_eq!(
            after["lsp"]["applicability_by_root"]
                .as_object()
                .unwrap()
                .len(),
            192
        );
    }

    #[test]
    fn status_push_narrow_builder_matches_full_snapshot_projection() {
        let fixture = busy_root();
        let expected = project(&fixture.ctx, fixture.ctx.build_status_snapshot());
        let pushed = fixture.ctx.try_build_status_push_snapshot().unwrap();
        assert_eq!(pushed, expected);
    }

    struct Refusals(Vec<(PersistedStore, PathBuf)>);
    impl Drop for Refusals {
        fn drop(&mut self) {
            for (store, path) in &self.0 {
                persisted_format::clear(*store, path);
            }
        }
    }

    #[test]
    fn status_push_refusals_preserve_owned_totals_and_unattributed_signal() {
        let fixture = busy_root();
        let mut guard = Refusals(Vec::new());
        for (key, count, store) in [
            (fixture.key.as_str(), 12, PersistedStore::SearchIndex),
            ("another-project", 40, PersistedStore::SearchIndex),
            ("unknown", 5, PersistedStore::BashTask),
        ] {
            for i in 0..count {
                let plane = if store == PersistedStore::BashTask {
                    "bash"
                } else {
                    "index"
                };
                let path = fixture
                    .storage
                    .join(plane)
                    .join(key)
                    .join(format!("artifact-{i}.bin"));
                let refusal = UnsupportedPersistedFormat::artifact(store, &path, 99);
                persisted_format::record(&refusal, &path);
                guard.0.push((store, path));
            }
        }
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(full["storage_refusals"].as_array().unwrap().len(), 57);
        let pushed = project(&fixture.ctx, full);
        assert_eq!(pushed["storage_refusals_total"], 12);
        assert_eq!(pushed["storage_refusals_unattributed"], 5);
        let shown = pushed["storage_refusals"].as_array().unwrap();
        assert!(!shown.is_empty() && shown.len() < 12);
        assert_eq!(
            pushed["storage_refusals_omitted"].as_u64().unwrap(),
            12 - shown.len() as u64
        );
        assert!(shown
            .iter()
            .all(|row| row["path"].as_str().unwrap().contains(&fixture.key)));
        assert_eq!(pushed["push_details"]["read"], "status");
        assert_eq!(pushed["degraded"], true);
        assert_eq!(
            fixture.ctx.build_status_snapshot()["storage_refusals"]
                .as_array()
                .unwrap()
                .len(),
            57
        );
    }

    #[test]
    fn status_push_consumer_field_inventory_is_preserved() {
        let fixture = busy_root();
        let mut full = fixture.ctx.build_status_snapshot();
        let fields: Vec<String> = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_fields.json"
        ))
        .unwrap();
        // Populate missing optional, error and legacy fields so the inventory
        // test verifies that the projection preserves them too.
        for field in &fields {
            if full.pointer(field).is_none() {
                let mut cursor = &mut full;
                let parts: Vec<_> = field.trim_start_matches('/').split('/').collect();
                for part in &parts[..parts.len() - 1] {
                    if !cursor.is_object() {
                        *cursor = serde_json::json!({});
                    }
                    cursor = cursor
                        .as_object_mut()
                        .unwrap()
                        .entry((*part).to_owned())
                        .or_insert(Value::Null);
                }
                if !cursor.is_object() {
                    *cursor = serde_json::json!({});
                }
                cursor[parts[parts.len() - 1]] = serde_json::json!(format!("inventory:{field}"));
            }
        }
        let frame = StatusChangedFrame::new(None, project(&fixture.ctx, full.clone()));
        for field in fields {
            assert_eq!(
                frame.snapshot.pointer(&field),
                Some(
                    full.pointer(&field)
                        .expect("all inventory fields populated")
                ),
                "consumer reads {field}"
            );
        }
        assert!(frame.snapshot.get("session").is_none());
        assert!(frame.snapshot["compression"].get("session").is_none());
    }

    #[test]
    fn status_push_consumer_fixture_matches_projection() {
        let fixture = busy_root();
        let full: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_sample.json"
        ))
        .unwrap();
        let frame = StatusChangedFrame::new(None, project(&fixture.ctx, full.clone()));
        let fields: Vec<String> = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_fields.json"
        ))
        .unwrap();
        for field in fields {
            assert_eq!(
                frame.snapshot.pointer(&field),
                Some(full.pointer(&field).expect("complete consumer fixture")),
                "consumer fixture reads {field}"
            );
        }
        let captured: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_projected.json"
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            captured,
            "shared consumer fixture must match the Rust producer"
        );
    }

    #[test]
    fn status_push_compacted_consumer_fixture_matches_backstop() {
        let fixture = busy_root();
        let full: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_sample.json"
        ))
        .unwrap();
        let mut snapshot = project(&fixture.ctx, full);
        for bar in ["status_bar", "status_bar_values"] {
            snapshot[bar]["disabled_categories"] = serde_json::json!(["dead_code", "todos"]);
            snapshot[bar]["diagnostics"] = serde_json::json!("producer_missing");
        }
        snapshot["semantic_index"]["error"] = serde_json::json!("\u{0000}".repeat(12_000));
        snapshot["storage_refusals_total"] = serde_json::json!(19);
        snapshot["storage_refusals_omitted"] = serde_json::json!(19);
        snapshot["storage_refusals_unattributed"] = serde_json::json!(7);
        let body = bounded_body(&StatusChangedFrame::new(None, snapshot));
        assert!(body.compacted);
        assert!(body.bytes.len() <= BODY_MAX_BYTES);
        let captured: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/status_push_consumer_compacted.json"
        ))
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body.bytes).unwrap(),
            captured,
            "shared compacted fixture must match the Rust backstop"
        );
    }

    #[test]
    fn status_push_compaction_marks_omitted_consumer_list_entries() {
        let snapshot = serde_json::json!({
            "degraded_reasons": (0..=COMPACTED_DEGRADED_REASONS)
                .map(|index| format!("reason_{index}"))
                .collect::<Vec<_>>(),
            "status_bar": {
                "disabled_categories": (0..=COMPACTED_DISABLED_CATEGORIES)
                    .map(|index| format!("category_{index}"))
                    .collect::<Vec<_>>(),
                "diagnostics_gaps": (0..=COMPACTED_DIAGNOSTIC_GAPS)
                    .map(|index| serde_json::json!({
                        "language": format!("language_{index}"),
                        "producer": format!("producer_{index}"),
                        "root": format!("/project/{index}"),
                    }))
                    .collect::<Vec<_>>(),
            },
        });

        let compacted = summary(&snapshot, 256);
        assert_eq!(
            compacted["degraded_reasons"].as_array().unwrap().len(),
            COMPACTED_DEGRADED_REASONS
        );
        assert_eq!(
            compacted["status_bar"]["disabled_categories"]
                .as_array()
                .unwrap()
                .len(),
            COMPACTED_DISABLED_CATEGORIES
        );
        assert_eq!(
            compacted["status_bar"]["diagnostics_gaps"]
                .as_array()
                .unwrap()
                .len(),
            COMPACTED_DIAGNOSTIC_GAPS
        );
        assert_eq!(
            compacted["status_push_list_omitted"],
            serde_json::json!({
                "degraded_reasons": 1,
                "status_bar.disabled_categories": 1,
                "status_bar.diagnostics_gaps": 1,
            })
        );
    }

    #[test]
    fn status_push_foreign_refusal_does_not_degrade_root() {
        let fixture = busy_root();
        let path = fixture
            .storage
            .join("index")
            .join("foreign-only")
            .join("cache.bin");
        let refusal = UnsupportedPersistedFormat::artifact(PersistedStore::SearchIndex, &path, 99);
        persisted_format::record(&refusal, &path);
        let _guard = Refusals(vec![(PersistedStore::SearchIndex, path)]);
        let full = fixture.ctx.build_status_snapshot();
        assert_eq!(full["degraded"], true);
        let pushed = project(&fixture.ctx, full);
        assert_eq!(pushed["degraded"], false);
        assert_eq!(pushed["storage_refusals_total"], 0);
        assert_eq!(pushed["storage_refusals_unattributed"], 0);
    }

    #[test]
    fn status_push_shared_storage_and_cached_artifact_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path();
        let mut guard = Refusals(Vec::new());
        for (store, path) in [
            (
                PersistedStore::SemanticIndex,
                storage.join("semantic/shared-key/semantic.bin"),
            ),
            (
                PersistedStore::SearchIndex,
                storage.join("index/other-key/cache.bin"),
            ),
            (
                PersistedStore::BashTask,
                storage.join("bash/unknown/task.json"),
            ),
            (PersistedStore::AftDb, storage.join("aft.db")),
        ] {
            persisted_format::record(
                &UnsupportedPersistedFormat::artifact(store, &path, 99),
                &path,
            );
            guard.0.push((store, path));
        }
        persisted_format::record(
            &UnsupportedPersistedFormat::floor(
                PersistedStore::AftDb,
                storage.join("reader-floor.json"),
                99,
            ),
            storage,
        );
        let (own, unknown) = persisted_format::refusals_for_push(storage, Some("shared-key"));
        assert_eq!(own.len(), 3);
        assert_eq!(unknown, 1);
        assert!(!own
            .iter()
            .any(|refusal| refusal.path.to_string_lossy().contains("other-key")));
        let (worktree, _) = persisted_format::refusals_for_push(storage, Some("shared-key"));
        assert_eq!(
            worktree, own,
            "repository artifacts can be shared by worktrees, not guessed from task session hashes"
        );
        let (uncached, unknown) = persisted_format::refusals_for_push(storage, None);
        assert_eq!(uncached.len(), 2);
        assert_eq!(unknown,3,"uncached artifact ownership remains unknown rather than deriving a key on the push path");
    }

    #[test]
    fn status_push_refusal_totals_deduplicate_artifacts_before_filtering() {
        let dir = tempfile::tempdir().unwrap();
        let a = UnsupportedPersistedFormat::artifact(
            PersistedStore::CallgraphStore,
            dir.path().join("callgraph/key/a.sqlite"),
            99,
        );
        let b = UnsupportedPersistedFormat::artifact(
            PersistedStore::CallgraphStore,
            dir.path().join("callgraph/key/b.sqlite"),
            99,
        );
        let scopes = [
            dir.path().join("scope-a"),
            dir.path().join("scope-b"),
            dir.path().join("scope-c"),
        ];
        for (refusal, scope) in [(&a, &scopes[0]), (&b, &scopes[1]), (&a, &scopes[2])] {
            persisted_format::record(refusal, scope);
        }
        let _guard = Refusals(
            scopes
                .into_iter()
                .map(|scope| (PersistedStore::CallgraphStore, scope))
                .collect(),
        );
        let (owned, unknown) = persisted_format::refusals_for_push(dir.path(), Some("key"));
        assert_eq!(
            owned.len(),
            2,
            "the same refused artifact can be observed through more than one scope"
        );
        assert_eq!(unknown, 0);
    }
}
