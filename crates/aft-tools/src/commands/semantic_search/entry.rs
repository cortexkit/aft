use super::*;

#[cfg(test)]
mod external_hook_tests;

pub fn handle_semantic_search(req: &RawRequest, ctx: &AppContext) -> Response {
    // Direct library callers do not have the binary's startup registration.
    crate::commands::register_external_search();
    crate::bounded_io::with_skip_disclosure(|| handle_semantic_search_with_disclosure(req, ctx))
}

fn handle_semantic_search_with_disclosure(req: &RawRequest, ctx: &AppContext) -> Response {
    let _cost = telemetry::CostRequest::start(&req.id);
    let mut response = handle_semantic_search_with_disk_fallback(req, ctx, false);
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
