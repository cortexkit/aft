use super::*;
use subc_config::{FieldChange, Layer};

const AFT_MODULE: &str = "aft";

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
                Err(path) => refusal(path),
            })
            .await
            .unwrap_or_else(|_| refusal(String::new()));
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

fn refusal(path: String) -> ConfigReviewReply {
    ConfigReviewReply::new(
        vec![],
        vec![ConfigReviewRefusal::new(
            "value_invalid",
            vec![path],
            vec!["Invalid AFT setting".into()],
        )],
    )
}

fn review(schema: &Schema, request: ConfigReviewRequest) -> Result<Vec<FieldChange>, String> {
    let mut layers = Vec::new();
    for layer in request.layers {
        if layer.layer != ConfigLayer::User
            || layer.project_id.is_some()
            || layer.workspace_id.is_some()
            || layer.module_id != AFT_MODULE
        {
            return Err(String::new());
        }
        if let Some(bytes) = layer.bytes {
            let text = std::str::from_utf8(&bytes).map_err(|_| String::new())?;
            let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(text))
                .map_err(|_| String::new())?;
            layers.push(LayerValue::new(Layer::User, value));
        }
    }
    let tuple = schema
        .review_tuple(&layers, &ValidationContext::new(AFT_MODULE, None))
        .map_err(|error| error.path)?;
    validate_user(&tuple.effective)?;
    let new_values = schema.record(&tuple.effective).map_err(|_| String::new())?;
    let base = request.base;
    let paths: std::collections::BTreeSet<_> = tuple
        .sources
        .keys()
        .chain(base.iter().flat_map(|view| view.sources.keys()))
        .collect();
    let mut fields = Vec::new();
    for path in paths {
        let old = base
            .as_ref()
            .and_then(|view| view.effective.pointer(path))
            .unwrap_or(&Value::Null);
        let new = new_values.pointer(path).unwrap_or(&Value::Null);
        let old_source = base
            .as_ref()
            .and_then(|view| view.sources.get(path))
            .copied()
            .unwrap_or(Layer::Default);
        let new_source = tuple.sources.get(path).copied().unwrap_or(Layer::Default);
        if old == new && old_source == new_source {
            continue;
        }
        let key = path.trim_start_matches('/').replace('/', ".");
        let apply = if crate::config_live::restart_required(&key) {
            ApplyKind::ModuleRestart
        } else {
            ApplyKind::Live
        };
        let mut change = FieldChange::new(AFT_MODULE, path, recorded(old), recorded(new), apply);
        change.old_source = old_source;
        change.new_source = new_source;
        fields.push(change);
    }
    Ok(fields)
}

fn recorded(value: &Value) -> RecordedValue {
    // Schema::record already replaces credential leaves with digest records.
    if value.get("redacted") == Some(&Value::Bool(true)) {
        let mut record = value.clone();
        record["kind"] = json!("redacted");
        if let Ok(record) = serde_json::from_value(record) {
            return record;
        }
    }
    RecordedValue::new(value.clone())
}
