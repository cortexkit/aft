//! Inspect for parent folder sessions.
//!
//! The project-wide categories (dead code, unused exports, duplicates and the
//! other Tier-2 analyses) are read from each child's persisted inspect
//! aggregates through a read-only handle the worker opened once, then merged:
//! counts add up and item lists concatenate with parent-relative paths. A
//! child without a current aggregate is a named gap. The per-file categories
//! (diagnostics, metrics, todos) are computed by a session bound to the
//! repository itself, so the parent names them as gaps instead of starting
//! language servers or scanning children on the request path.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::context::AppContext;
use crate::inspect::job::{InspectCategory, JobKey};
use crate::protocol::{RawRequest, Response};

use super::query::{attach_gaps, Gaps};
use super::{display_relative, read, ParentSession};

/// Adds `child` into `merged`: numbers are summed, arrays concatenated (with
/// paths made parent-relative), and nested objects merged the same way.
pub(super) fn merge(merged: &mut Map<String, Value>, child: &Map<String, Value>, prefix: &Path) {
    for (key, value) in child {
        match (merged.get_mut(key), value) {
            (Some(Value::Number(total)), Value::Number(add)) => {
                if let (Some(left), Some(right)) = (total.as_u64(), add.as_u64()) {
                    *total = (left + right).into();
                } else if let (Some(left), Some(right)) = (total.as_f64(), add.as_f64()) {
                    if let Some(sum) = serde_json::Number::from_f64(left + right) {
                        *total = sum;
                    }
                }
            }
            (Some(Value::Array(items)), Value::Array(add)) => {
                items.extend(add.iter().cloned().map(|mut item| {
                    prefix_value(&mut item, prefix);
                    item
                }));
            }
            (Some(Value::Object(inner)), Value::Object(add)) => merge(inner, add, prefix),
            (Some(Value::Bool(flag)), Value::Bool(add)) => *flag |= add,
            (Some(_), _) => {}
            (None, value) => {
                let mut value = value.clone();
                prefix_value(&mut value, prefix);
                merged.insert(key.clone(), value);
            }
        }
    }
}

/// Makes checkout-relative `file`/`path` strings parent-relative.
fn prefix_value(value: &mut Value, prefix: &Path) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                match value {
                    Value::String(text) if matches!(key.as_str(), "file" | "path") => {
                        if !Path::new(text.as_str()).is_absolute() {
                            *text = prefix
                                .join(text.as_str())
                                .to_string_lossy()
                                .replace('\\', "/");
                        }
                    }
                    other => prefix_value(other, prefix),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|item| prefix_value(item, prefix)),
        _ => {}
    }
}

fn requested_categories(req: &RawRequest) -> Vec<InspectCategory> {
    let named = match req.params.get("sections") {
        Some(Value::String(name)) => vec![name.clone()],
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let active = InspectCategory::active();
    if named.is_empty() || named.iter().any(|name| name == "all") {
        return active.to_vec();
    }
    active
        .iter()
        .copied()
        .filter(|category| named.iter().any(|name| name == category.as_str()))
        .collect()
}

fn requested_scopes(
    req: &RawRequest,
    ctx: &AppContext,
    session: &ParentSession,
) -> Result<Vec<PathBuf>, Response> {
    let raw = match req.params.get("scope") {
        Some(Value::String(path)) => vec![path.clone()],
        Some(Value::Array(paths)) => paths
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => return Ok(vec![session.root().to_path_buf()]),
    };
    raw.iter()
        .map(|path| {
            let validated = ctx.validate_path(&req.id, Path::new(path))?;
            let absolute = if validated.is_relative() {
                session.root().join(validated)
            } else {
                validated
            };
            Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
        })
        .collect()
}

pub(super) fn answer(req: &RawRequest, ctx: &AppContext, session: &ParentSession) -> Response {
    let scopes = match requested_scopes(req, ctx, session) {
        Ok(scopes) => scopes,
        Err(response) => return response,
    };
    let categories = requested_categories(req);
    let config = ctx.config();
    let mut gaps = Gaps::default();
    let mut summary = Map::new();
    for category in &categories {
        summary.insert(
            category.as_str().into(),
            if config.inspect.category_enabled(*category) {
                Value::Object(Map::new())
            } else {
                json!({"off":true,"complete":true})
            },
        );
    }
    let children = session
        .children()
        .into_iter()
        .filter(|child| {
            scopes
                .iter()
                .any(|scope| child.root.starts_with(scope) || scope.starts_with(&child.root))
        })
        .collect::<Vec<_>>();
    for child in &children {
        let cache = read(&child.inspect).clone();
        for category in &categories {
            if !config.inspect.category_enabled(*category) {
                continue;
            }
            if !category.is_tier2() {
                gaps.push(
                    "parent_inspect_category",
                    child.display(),
                    format!(
                        "{}: computed only by a session opened in this repository",
                        category.as_str()
                    ),
                );
                continue;
            }
            let aggregate = cache.as_ref().map(|cache| {
                cache.get_aggregated_for_config(&JobKey::for_project_category(*category), &config)
            });
            match aggregate {
                Some(Ok(Some(Value::Object(payload)))) => {
                    if let Some(Value::Object(merged)) = summary.get_mut(category.as_str()) {
                        merge(merged, &payload, &child.relative);
                    }
                }
                Some(Err(error)) => gaps.push(
                    "parent_child_unavailable",
                    child.display(),
                    format!("{}: inspect results unreadable: {error}", category.as_str()),
                ),
                _ => gaps.push(
                    "parent_child_unavailable",
                    child.display(),
                    format!(
                        "{}: no current inspect results for this repository yet",
                        category.as_str()
                    ),
                ),
            }
        }
    }
    for scope in &scopes {
        gaps.scope(session, scope);
    }
    let mut lines = vec![format!(
        "Parent folder {}: {} repositories in scope.",
        display_relative(session.root(), session.root()),
        children.len()
    )];
    for (category, merged) in &summary {
        if merged["off"] == true {
            lines.push(format!(
                "{}: off (inspect.categories.{category})",
                category.replace('_', " ")
            ));
            continue;
        }
        let count = merged
            .get("count")
            .or_else(|| merged.get("total"))
            .and_then(Value::as_u64);
        lines.push(match count {
            Some(count) => format!("{category}: {count}"),
            None => format!("{category}: see summary"),
        });
    }
    let mut body = json!({
        "summary": summary,
        "text": lines.join("\n"),
        "scope_roots": scopes
            .iter()
            .map(|scope| display_relative(session.root(), scope))
            .collect::<Vec<_>>(),
        "complete": true,
    });
    attach_gaps(&mut body, gaps.into_values());
    Response::success(&req.id, body)
}
