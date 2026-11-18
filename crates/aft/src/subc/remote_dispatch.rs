//! The remote read allowlist is independent of the local and confined catalogs.

use super::*;
use crate::db::remote_grant_rows::Mode;
use cortexkit_role_tool_provider::call::ToolCallRequest;
use cortexkit_role_tool_provider::errors;

pub(super) fn read_operation(wire_name: &str) -> Option<&'static str> {
    match crate::tool_gate::canonical_tool_name(wire_name)? {
        "read" => Some("read"),
        "grep" => Some("grep"),
        "glob" => Some("glob"),
        "aft_search" => Some("search"),
        "aft_outline" => Some("outline"),
        "aft_zoom" => Some("zoom"),
        "aft_callgraph" => Some("callgraph"),
        "ast_grep_search" => Some("ast_search"),
        "aft_conflicts" => Some("conflicts"),
        _ => None,
    }
}

pub(super) struct Read {
    operation: &'static str,
    arguments: Value,
    context: ToolCallContext,
}

impl Read {
    pub(super) fn prepare(
        call: ToolCallRequest,
        scope: ScopeStamp,
        folder: PathBuf,
        session: String,
        digest: String,
        ctx: &AppContext,
    ) -> Result<(Self, Preflight), ErrorBody> {
        let operation = read_operation(&call.name).ok_or_else(tool_unavailable)?;
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce).map_err(|_| admission_refusal())?;
        let request_id = format!("remote-{:032x}", u128::from_le_bytes(nonce));
        let context = ToolCallContext {
            project_root: folder.clone(),
            // File-tool policy, artifact restores and repeat observations must
            // not borrow an existing local session with a caller-chosen name.
            session_id: Some(request_id.clone()),
            request_id,
            standard_edit_grammar: true,
            ..ToolCallContext::default()
        };
        let read = Self {
            operation,
            arguments: call.arguments,
            context,
        };
        crate::confinement::with_root(ctx, &read.context.request_id, || {
            read.check(ctx)?;
            let format = read.format();
            let prepared = prepare_tool_call(
                read.operation,
                read.arguments.clone(),
                &format,
                &read.context,
                ctx,
                None,
            )
            .map_err(|failed| failed.response)?;
            if read.arguments.get("ref").is_some() {
                crate::revision_read::check_enabled(&prepared.request, ctx)?;
            }
            Ok(())
        })
        .map_err(|refusal| error(&refusal))?;
        let consent = Preflight {
            scope,
            session,
            folder,
            mode: Mode::Read,
            tool: call.name,
            args_hash: digest,
        };
        Ok((read, consent))
    }

    fn format(&self) -> crate::subc_format::FormatContext {
        crate::subc_format::FormatContext::from_tool_call(
            self.operation,
            &self.arguments,
            &self.context.project_root,
        )
    }

    fn check(&self, ctx: &AppContext) -> Result<(), Response> {
        let id = &self.context.request_id;
        if let Some(refusal) =
            crate::tool_gate::refusal_by_name(id, self.operation, &ctx.config().disabled_tools)
        {
            return Err(refusal);
        }
        let session = self
            .context
            .session_id
            .as_deref()
            .expect("remote file-tool session");
        crate::confinement::preflight(ctx, id, session, &self.arguments)?;
        if let Some(paths) = self.arguments.get("paths") {
            crate::confinement::preflight(ctx, id, session, &json!({"path": paths}))?;
        }
        if let Some(batch) = self.arguments["batch"].as_array() {
            for item in batch {
                crate::confinement::preflight(ctx, id, session, item)?;
            }
        }
        Ok(())
    }

    pub(super) fn run(
        self,
        ctx: &AppContext,
        dispatch: DispatchFn,
    ) -> Result<ToolCallResult, ErrorBody> {
        let _config = ctx.pin_config();
        let format = self.format();
        let id = self.context.request_id.clone();
        crate::confinement::with_root(ctx, &id, || {
            // Consent and scheduling may have delayed this call. Recheck the
            // current path and tool policy immediately before dispatch.
            self.check(ctx)?;
            let crate::run_tool_call::ToolCallOutcome::Unary(result) = run_tool_call(
                self.operation,
                self.arguments,
                &format,
                &self.context,
                ctx,
                &dispatch,
                None,
                None,
            );
            Ok(result)
        })
        .map_err(|refusal| error(&refusal))
    }
}

fn error(refusal: &Response) -> ErrorBody {
    let code = refusal.data["code"]
        .as_str()
        .unwrap_or(errors::INVALID_REQUEST);
    match code {
        crate::tool_gate::TOOL_DISABLED_CODE => tool_unavailable(),
        "path_outside_confined_root" => ErrorBody::new(
            "path_outside_root",
            response_message(refusal, "Path is outside the project root."),
        ),
        _ => ErrorBody::new(
            code,
            response_message(refusal, "The tool request was refused."),
        ),
    }
}
