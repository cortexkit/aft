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
            let value: Value = serde_json::from_str(&crate::jsonc::strip_jsonc(
                text.trim_start_matches('\u{feff}'),
            ))
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
    let old_values = base
        .as_ref()
        .map(|view| schema.record(&view.effective))
        .transpose()
        .map_err(|_| String::new())?;
    let mut paths = std::collections::BTreeSet::new();
    leaf_paths(&tuple.effective, "", &mut paths);
    if let Some(base) = &base {
        leaf_paths(&base.effective, "", &mut paths);
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
        let apply = if crate::config_live::restart_required(key) {
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
        change.old_source = old_source;
        change.new_source = new_source;
        fields.push(change);
    }
    Ok(fields)
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

fn source(sources: &std::collections::BTreeMap<String, Layer>, path: &str) -> Option<Layer> {
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

fn leaf_paths(value: &Value, path: &str, paths: &mut std::collections::BTreeSet<String>) {
    if let Some(fields) = value.as_object().filter(|fields| {
        !fields.is_empty() && !(fields.len() == 1 && fields.contains_key("vault_ref"))
    }) {
        for (name, value) in fields {
            leaf_paths(
                value,
                &format!("{path}/{}", name.replace('~', "~0").replace('/', "~1")),
                paths,
            );
        }
    } else if !path.is_empty() {
        paths.insert(path.into());
    }
}
