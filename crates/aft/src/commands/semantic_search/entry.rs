use super::*;

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    crate::bounded_io::with_skip_disclosure(|| handle_semantic_search_with_disclosure(req, ctx))
}

fn handle_semantic_search_with_disclosure(req: &RawRequest, ctx: &AppContext) -> Response {
    let _cost = telemetry::CostRequest::start(&req.id);
    let mut response = handle_semantic_search_with_disk_fallback(req, ctx, false, None);
    if let Some(reason) = ctx.cache_unavailable_reason() {
        let code = crate::root_cache::cache_status_code(&reason);
        response.data["cache_unavailable_reason"] = serde_json::json!(code);
        if let Some(output) = response.data["text"].as_str() {
            // A busy reason already starts with its code; do not repeat it.
            let line = if reason.starts_with(code) {
                reason.clone()
            } else {
                format!("{code}: {reason}")
            };
            response.data["text"] = serde_json::json!(format!("{line}\n\n{output}"));
        }
    }
    disclose_skipped_folders(req, ctx, &mut response);
    response
}

pub(crate) struct SearchInput<'a> {
    pub(crate) query: &'a str,
    pub(crate) pattern: Option<(&'a str, pattern_compile::CompiledPattern)>,
}

impl<'a> SearchInput<'a> {
    pub(crate) fn parse(req: &'a RawRequest) -> Result<Self, Response> {
        let query = req.params["query"].as_str().unwrap_or_default();
        let pattern = pattern_param(req)?;
        if query.trim().is_empty() && pattern.is_none() {
            return Err(Response::error(
                &req.id,
                "invalid_request",
                if req.params.get("pattern").is_some() {
                    "at least one of query or pattern must be non-empty"
                } else {
                    "query must be non-empty"
                },
            ));
        }
        Ok(Self { query, pattern })
    }
}

// Compile with grep's case-sensitive default and share its refusals.
fn pattern_param(
    req: &RawRequest,
) -> Result<Option<(&str, pattern_compile::CompiledPattern)>, Response> {
    let pattern = match req.params.get("pattern") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(pattern)) => pattern,
        Some(_) => {
            return Err(Response::error(
                &req.id,
                "invalid_request",
                "pattern must be a string",
            ));
        }
    };
    if pattern.trim().is_empty() {
        return Ok(None);
    }
    match pattern_compile::compile(pattern, CompileOpts::default()) {
        CompileResult::Ok(compiled) => Ok(Some((pattern, compiled))),
        CompileResult::InvalidPattern { message, .. } => Err(Response::error_with_data(
            &req.id,
            "invalid_pattern",
            message,
            serde_json::json!({ "pattern": pattern }),
        )),
        CompileResult::UnsupportedSyntax { feature, .. } => Err(Response::error_with_data(
            &req.id,
            "unsupported_pattern",
            format!(
                "Pattern uses regex syntax not supported by AFT's engine: {feature}. Rewrite without {feature} or use grep for explicit regex control."
            ),
            serde_json::json!({ "pattern": pattern, "feature": feature }),
        )),
    }
}
