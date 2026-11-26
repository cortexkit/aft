use super::*;

pub fn handle_inspect(req: &RawRequest, ctx: &AppContext) -> Response {
    if let Some(reason) = ctx.cache_unavailable_reason() {
        return Response::error(&req.id, crate::root_cache::CACHE_DIR_UNAVAILABLE, reason);
    }
    let _activity = ctx.lsp_diagnostics_activity();
    if req.worker_session() {
        ctx.disable_rust_cargo_for_worker();
    }
    handle_inspect_payload(
        req,
        ctx,
        false,
        false,
        &[],
        &[],
        &[],
        &[],
        false,
        None,
        None,
        None,
        Vec::new(),
    )
}
