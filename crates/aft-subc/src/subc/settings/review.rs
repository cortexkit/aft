use super::*;
use subc_config::{FieldChange, Layer};

const AFT_MODULE: &str = "aft";
const INVALID_SETTING: &str = "Invalid AFT setting";
const RERANK_REFUSAL: &str = "A project or workspace file can only set search reranking to off.";
const CHANGED_FILES_REFUSAL: &str =
    "A project or workspace file can only set bash changed-file reporting to off.";
const ENV_REFUSAL: &str = "A project or workspace file can't set environment variables that choose which programs run, which libraries load, or how the shell starts.";

pub(super) async fn response(
    schema: &Arc<Schema>,
    frame: &Frame,
) -> Result<Option<Frame>, SubcError> {
    if frame.header.channel != 0 || frame.header.epoch != 0 || frame.header.ty != FrameType::Request
    {
        return Ok(None);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&frame.body) else {
        return Ok(None);
    };
    if value.get("op").and_then(Value::as_str) != Some(CONFIG_REVIEW_OP) {
        return Ok(None);
    }
    let (kind, body) = match serde_json::from_value::<ConfigReviewRequest>(value) {
        Ok(request) => {
            let schema = schema.clone();
            let reply = tokio::task::spawn_blocking(move || match review(&schema, request) {
                Ok(fields) => ConfigReviewReply::new(fields, vec![]),
                Err(refusal) => ConfigReviewReply::new(vec![], vec![refusal]),
            })
            .await
            .unwrap_or_else(|_| ConfigReviewReply::new(vec![], vec![invalid(String::new())]));
            (FrameType::Response, serde_json::to_vec(&reply))
        }
        Err(_) => (
            FrameType::Error,
            serde_json::to_vec(&ErrorBody::new(
                "invalid_request",
                "Invalid settings review snapshots",
            )),
        ),
    };
    Frame::build_with_version(
        frame.header.ver,
        kind,
        control_flags(),
        0,
        0,
        frame.header.corr,
        body.map_err(SubcError::Json)?,
    )
    .map(Some)
    .map_err(SubcError::FrameBuild)
}

fn invalid(path: String) -> ConfigReviewRefusal {
    ConfigReviewRefusal::new("value_invalid", vec![path], vec![INVALID_SETTING.into()])
}

fn review(
    schema: &Schema,
    request: ConfigReviewRequest,
) -> Result<Vec<FieldChange>, ConfigReviewRefusal> {
    let invalid_snapshot = || invalid(String::new());
    let project_id = request
        .base
        .as_ref()
        .and_then(|base| base.project_id.clone())
        .or_else(|| {
            request
                .layers
                .iter()
                .filter(|snapshot| snapshot.layer != ConfigLayer::User)
                .find_map(|snapshot| snapshot.project_id.clone())
        });
    let mut layers = Vec::new();
    for snapshot in request.layers {
        if snapshot.module_id != AFT_MODULE {
            return Err(invalid_snapshot());
        }
        let layer = match snapshot.layer {
            ConfigLayer::User if snapshot.workspace_id.is_none() => Layer::User,
            ConfigLayer::Workspace if snapshot.workspace_id.is_some() => Layer::Workspace,
            ConfigLayer::Project
                if snapshot.project_id.is_some() && snapshot.workspace_id.is_none() =>
            {
                Layer::Project
            }
            _ => return Err(invalid_snapshot()),
        };
        if snapshot.project_id.is_some() && snapshot.project_id != project_id {
            return Err(invalid_snapshot());
        }
        if let Some(bytes) = snapshot.bytes {
            let value = parse_snapshot(&bytes).ok_or_else(invalid_snapshot)?;
            if layer != Layer::User {
                if let Some((path, reason)) = forbidden_override(&value) {
                    let mut refusal =
                        ConfigReviewRefusal::new("value_invalid", vec![path], vec![reason.into()]);
                    refusal.project_id = project_id.clone();
                    return Err(refusal);
                }
            }
            layers.push(LayerValue::new(layer, value));
        }
    }
    let context = ValidationContext::new(AFT_MODULE, project_id);
    let tuple = schema
        .review_tuple(&layers, &context)
        .map_err(|error| invalid(error.path))?;
    validate_harness_bounds(schema, &layers, &context).map_err(invalid)?;
    validate_user(&tuple.effective).map_err(invalid)?;
    let new_values = schema
        .record(&tuple.effective)
        .map_err(|_| invalid_snapshot())?;
    let base = request.base;
    let old_values = base
        .as_ref()
        .map(|view| schema.record(&view.effective))
        .transpose()
        .map_err(|_| invalid_snapshot())?;
    let mut paths = std::collections::BTreeSet::new();
    leaf_paths(&tuple.effective, "", project_id.is_some(), &mut paths);
    if let Some(base) = &base {
        leaf_paths(&base.effective, "", project_id.is_some(), &mut paths);
    }
    let mut fields = Vec::new();
    for path in paths {
        let old = base.as_ref().and_then(|view| view.effective.pointer(&path));
        let new = tuple.effective.pointer(&path);
        let old_source = base
            .as_ref()
            .and_then(|view| source(&view.sources, &path))
            .unwrap_or(Layer::Default);
        let new_source = source(&tuple.sources, &path).unwrap_or(Layer::Default);
        if old == new && old_source == new_source {
            continue;
        }
        let key = path.trim_start_matches('/').replace('/', ".");
        // The declaration classifies a harness override by the root-relative
        // key it overrides, so `harnesses.<name>.disabled_tools` applies like
        // `disabled_tools`.
        let key = match key.strip_prefix("harnesses.") {
            Some(rest) => rest.split_once('.').map_or("", |(_, field)| field),
            None => key.as_str(),
        };
        let apply = if crate::config_live::schema::settings_restart_required(key) {
            ApplyKind::ModuleRestart
        } else {
            ApplyKind::Live
        };
        let mut change = FieldChange::new(
            AFT_MODULE,
            &path,
            recorded(old_values.as_ref().and_then(|value| value.pointer(&path))),
            recorded(new_values.pointer(&path)),
            apply,
        );
        change.project_id.clone_from(&project_id);
        change.old_source = old_source;
        change.new_source = new_source;
        fields.push(change);
    }
    Ok(fields)
}

pub(super) fn parse_snapshot(bytes: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(bytes)
        .ok()?
        .trim_start_matches('\u{feff}');
    serde_json::from_str(&crate::jsonc::strip_jsonc(text)).ok()
}

// The schema compares layers at identical pointers. A harness setting also
// inherits the root value, so check the projected settings for that harness.
pub(super) fn validate_harness_bounds(
    schema: &Schema,
    layers: &[LayerValue],
    context: &ValidationContext,
) -> Result<(), String> {
    if !layers.iter().any(|layer| layer.layer != Layer::User) {
        return Ok(());
    }
    let harnesses: std::collections::BTreeSet<_> = layers
        .iter()
        .filter_map(|layer| layer.value.get("harnesses")?.as_object())
        .flat_map(|harnesses| harnesses.keys())
        .collect();
    for name in harnesses {
        let inputs: Vec<_> = layers
            .iter()
            .map(|layer| {
                let mut value = layer.value.clone();
                if let Some(overrides) = value
                    .as_object_mut()
                    .and_then(|root| root.remove("harnesses"))
                {
                    if let Some(overrides) = overrides.get(name) {
                        merge_objects(&mut value, overrides);
                    }
                }
                LayerValue::new(layer.layer, value)
            })
            .collect();
        if let Err(error) = schema.review_tuple_attributed(&inputs, context) {
            let path = error.refusal.path;
            if error.refusal.code == subc_config::RefusalCode::BoundViolated {
                let prefix = format!("/harnesses/{}", pointer_segment(name));
                if layers.iter().any(|layer| {
                    Some(layer.layer) == error.layer
                        && layer.value.pointer(&format!("{prefix}{path}")).is_some()
                }) {
                    return Err(format!("{prefix}{path}"));
                }
                return Err(path);
            }
        }
    }
    Ok(())
}

fn merge_objects(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base), Value::Object(overrides)) => {
            for (key, value) in overrides {
                merge_objects(base.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (base, value) => *base = value.clone(),
    }
}

pub(super) fn forbidden_override(value: &Value) -> Option<(String, &'static str)> {
    fn check(value: &Value, prefix: &str) -> Option<(String, &'static str)> {
        for (path, reason) in [
            ("/search/rerank/backend", RERANK_REFUSAL),
            ("/bash/changed_files", CHANGED_FILES_REFUSAL),
        ] {
            if value.pointer(path).is_some_and(|setting| setting != "off") {
                return Some((format!("{prefix}{path}"), reason));
            }
        }
        let env = value.pointer("/bash/env")?.as_object()?;
        env.keys()
            .find(|name| {
                let spelling = name.to_ascii_uppercase();
                matches!(
                    spelling.as_str(),
                    "PATH" | "LD_PRELOAD" | "LD_LIBRARY_PATH" | "BASH_ENV" | "ENV"
                ) || spelling.starts_with("DYLD_")
            })
            .map(|name| {
                (
                    format!("{prefix}/bash/env/{}", pointer_segment(name)),
                    ENV_REFUSAL,
                )
            })
    }

    check(value, "").or_else(|| {
        value
            .get("harnesses")?
            .as_object()?
            .iter()
            .find_map(|(harness, block)| {
                check(block, &format!("/harnesses/{}", pointer_segment(harness)))
            })
    })
}

pub(super) fn pointer_segment(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn recorded(value: Option<&Value>) -> RecordedValue {
    // Schema::record already replaces credential leaves with digest records.
    let value = value.cloned().unwrap_or(Value::Null);
    if value.get("kind").and_then(Value::as_str) == Some("redacted") {
        if let Ok(record) = serde_json::from_value(value.clone()) {
            return record;
        }
    }
    RecordedValue::new(value)
}

pub(super) fn source(
    sources: &std::collections::BTreeMap<String, Layer>,
    path: &str,
) -> Option<Layer> {
    // Arrays are reported as one change; the source map can name their items.
    if let Some((_, source)) = sources
        .range(format!("{path}/")..)
        .next()
        .filter(|(key, _)| key.starts_with(&format!("{path}/")))
    {
        return Some(*source);
    }
    let mut path = path;
    loop {
        if let Some(source) = sources.get(path) {
            return Some(*source);
        }
        path = path.rsplit_once('/')?.0;
    }
}

fn leaf_paths(
    value: &Value,
    path: &str,
    project: bool,
    paths: &mut std::collections::BTreeSet<String>,
) {
    if project && path == "/storage/cache_dir" {
        // The daemon records this opaque, user-only setting as one value even
        // when it is inherited by the project.
        paths.insert(path.into());
        return;
    }
    if let Some(fields) = value.as_object().filter(|fields| {
        !fields.is_empty() && !(fields.len() == 1 && fields.contains_key("vault_ref"))
    }) {
        for (name, value) in fields {
            leaf_paths(
                value,
                &format!("{path}/{}", pointer_segment(name)),
                project,
                paths,
            );
        }
    } else if !path.is_empty() {
        paths.insert(path.into());
    }
}
