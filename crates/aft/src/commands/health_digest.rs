//! Passive management operation for retrieving verified health values.
//!
//! The operation intentionally has no agent-tool registration. Sources that cannot
//! supply their freshness ticket are omitted instead of being represented as a
//! clean count or an empty change.

use serde::Serialize;
use serde_json::{Map, Value};

use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

/// Canonical management-operation name. Agent-facing tool registries must not
/// use this constant: discovery and authorization are a separate surface.
pub const HEALTH_DIGEST_OPERATION: &str = "health.digest";

/// Evidence that permits a current value to be returned without claiming an
/// interval relationship to a caller-provided anchor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FreshnessTicket {
    /// A diagnostic snapshot tagged with the document version it describes.
    DocumentVersion { version: i32 },
    /// A cached inspection artifact whose identity was stat-verified.
    ArtifactGeneration { identity: String, generation: u64 },
    /// A watcher journal event accepted for a stat-verified artifact identity.
    WatcherJournal { identity: String, sequence: u64 },
}

/// A value may only be included with the evidence that made it safe to read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TicketedCurrent<T> {
    pub value: T,
    pub ticket: FreshnessTicket,
}

impl<T> TicketedCurrent<T> {
    pub fn new(value: T, ticket: FreshnessTicket) -> Self {
        Self { value, ticket }
    }
}

/// The interim digest is deliberately a set of independent optional current
/// values. Its shape has no interval, comparison, or history fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DigestCurrentValues {
    pub errors: Option<TicketedCurrent<u64>>,
    pub dead_code: Option<TicketedCurrent<u64>>,
    pub unused_exports: Option<TicketedCurrent<u64>>,
    pub duplicates: Option<TicketedCurrent<u64>>,
    pub complexity_over_threshold: Option<TicketedCurrent<u64>>,
    pub todos: Option<TicketedCurrent<u64>>,
    pub watcher_events: Option<TicketedCurrent<u64>>,
    pub views: Option<TicketedCurrent<crate::context::ViewHealthSnapshot>>,
}

/// Render only independently verified current values. The caller owns source
/// verification; an absent source stays absent in the structured reply.
pub fn render_current_values(values: &DigestCurrentValues) -> Value {
    let mut fields = Map::new();
    insert_ticketed(&mut fields, "errors", values.errors.as_ref());
    insert_ticketed(&mut fields, "dead_code", values.dead_code.as_ref());
    insert_ticketed(
        &mut fields,
        "unused_exports",
        values.unused_exports.as_ref(),
    );
    insert_ticketed(&mut fields, "duplicates", values.duplicates.as_ref());
    insert_ticketed(
        &mut fields,
        "complexity_over_threshold",
        values.complexity_over_threshold.as_ref(),
    );
    insert_ticketed(&mut fields, "todos", values.todos.as_ref());
    insert_ticketed(
        &mut fields,
        "watcher_events",
        values.watcher_events.as_ref(),
    );
    insert_ticketed(&mut fields, "views", values.views.as_ref());
    Value::Object(fields)
}

fn insert_ticketed<T: Serialize>(
    fields: &mut Map<String, Value>,
    name: &str,
    value: Option<&TicketedCurrent<T>>,
) {
    let Some(value) = value else {
        return;
    };

    // `TicketedCurrent` contains only serializable primitive data and the
    // operation's fixed ticket enum. If that ever changes, omitting the field is
    // safer than reporting a value without its proof.
    if let Ok(value) = serde_json::to_value(value) {
        fields.insert(name.to_string(), value);
    }
}

/// Handle the management operation without starting analyzers, waiting for
/// quiescence, or constructing inspection work. Existing caches do not expose
/// the required freshness tickets yet, so every category is omitted here.
pub fn handle_health_digest(req: &RawRequest, ctx: &AppContext) -> Response {
    // `project_root` is the management-wire spelling; `root` remains accepted
    // for the standalone handler's original contract.
    let _conceptual_inputs = (
        req.params
            .get("project_root")
            .or_else(|| req.params.get("root")),
        req.params.get("since"),
    );

    let views = ticket_current_view(
        ctx.view_health_snapshot(),
        ctx.view_runtime_snapshot().map(|view| view.scope),
    );
    Response::success(
        &req.id,
        render_current_values(&DigestCurrentValues {
            views,
            ..DigestCurrentValues::default()
        }),
    )
}

fn ticket_current_view(
    value: Option<crate::context::ViewHealthSnapshot>,
    identity: Option<String>,
) -> Option<TicketedCurrent<crate::context::ViewHealthSnapshot>> {
    let value = value.filter(|value| value.generation > 0)?;
    let identity = identity.filter(|identity| !identity.is_empty())?;
    let generation = value.generation;
    Some(TicketedCurrent::new(
        value,
        FreshnessTicket::ArtifactGeneration {
            identity,
            generation,
        },
    ))
}

/// Preserve the operation's structured failure when its requested root has no
/// registered actor. Management callers can observe the miss without creating one.
pub fn root_not_bound_response(req: &RawRequest, root: &str) -> Response {
    Response::error(
        &req.id,
        "root_not_bound",
        format!("health.digest root is not bound: {root}"),
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{
        handle_health_digest, render_current_values, ticket_current_view, DigestCurrentValues,
        FreshnessTicket, TicketedCurrent, HEALTH_DIGEST_OPERATION,
    };
    use crate::config::Config;
    use crate::context::{callgraph_cold_build_spawn_count_for_test, AppContext};
    use crate::language::StubProvider;
    use crate::protocol::RawRequest;

    fn request() -> RawRequest {
        serde_json::from_value(json!({
            "id": "digest",
            "command": HEALTH_DIGEST_OPERATION,
        }))
        .expect("digest request is valid")
    }

    #[test]
    fn missing_view_identity_omits_the_ticket_instead_of_substituting_an_empty_name() {
        let snapshot = crate::context::ViewHealthSnapshot {
            generation: 7,
            pinned: true,
            pending_paths: 0,
            failed_paths: 0,
        };
        assert_eq!(ticket_current_view(Some(snapshot.clone()), None), None);
        assert_eq!(
            ticket_current_view(Some(snapshot.clone()), Some(String::new())),
            None
        );
        assert!(ticket_current_view(Some(snapshot), Some("view-scope".to_string())).is_some());
    }

    #[test]
    fn renders_only_ticketed_current_values() {
        let values = DigestCurrentValues {
            errors: Some(TicketedCurrent::new(
                2,
                FreshnessTicket::DocumentVersion { version: 7 },
            )),
            dead_code: Some(TicketedCurrent::new(
                3,
                FreshnessTicket::ArtifactGeneration {
                    identity: "artifact-a".to_string(),
                    generation: 11,
                },
            )),
            complexity_over_threshold: Some(TicketedCurrent::new(
                1,
                FreshnessTicket::ArtifactGeneration {
                    identity: "artifact-complexity".to_string(),
                    generation: 12,
                },
            )),
            ..DigestCurrentValues::default()
        };

        assert!(matches!(
            values.errors.as_ref().map(|current| &current.ticket),
            Some(FreshnessTicket::DocumentVersion { version: 7 })
        ));
        assert!(matches!(
            values.dead_code.as_ref().map(|current| &current.ticket),
            Some(FreshnessTicket::ArtifactGeneration { generation: 11, .. })
        ));

        let rendered = render_current_values(&values);
        let entries = rendered
            .as_object()
            .expect("ticketed values render as a structured object");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries["complexity_over_threshold"]["value"].as_u64(),
            Some(1)
        );

        let mut reported_values = entries
            .values()
            .filter_map(|entry| {
                entry
                    .as_object()
                    .and_then(|fields| fields.values().find_map(serde_json::Value::as_u64))
            })
            .collect::<Vec<_>>();
        reported_values.sort_unstable();
        assert_eq!(reported_values, vec![1, 2, 3]);
        assert!(entries.values().all(|entry| {
            entry
                .as_object()
                .is_some_and(|fields| fields.values().any(serde_json::Value::is_object))
        }));
    }

    #[test]
    fn omits_unverified_categories_without_placeholder_values() {
        let rendered = render_current_values(&DigestCurrentValues::default());
        assert_eq!(rendered, json!({}));
        let serialized = rendered.to_string();
        assert!(!serialized.contains('~'));
        assert!(!serialized.contains("delta"));
        assert!(!serialized.contains("changed"));
        assert!(!serialized.contains("resolved"));
        assert!(!serialized.contains("new"));
    }

    #[test]
    fn cold_context_is_passive_and_omits_diagnostics() {
        let root = tempfile::tempdir().expect("create project root");
        let mut config = Config::default();
        config.project_root = Some(PathBuf::from(root.path()));
        let ctx = AppContext::new(Box::new(StubProvider), config);
        let request = request();
        let server_count_before = ctx.lsp().server_count();
        let cold_build_count_before = callgraph_cold_build_spawn_count_for_test();
        let started = Instant::now();

        let response = handle_health_digest(&request, &ctx);

        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a passive digest must not wait for quiescence"
        );
        assert_eq!(ctx.lsp().server_count(), server_count_before);
        assert_eq!(
            callgraph_cold_build_spawn_count_for_test(),
            cold_build_count_before
        );
        assert_eq!(response.data, json!({}));
        assert!(response.data.get("diagnostics").is_none());
    }
}
