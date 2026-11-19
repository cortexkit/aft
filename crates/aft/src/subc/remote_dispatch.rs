//! The remote file-tool allowlist is independent of the local and confined catalogs.

use super::*;
use crate::context::confinement;
use crate::db::remote_grant_rows::Mode;
use cortexkit_role_tool_provider::call::ToolCallRequest;
use cortexkit_role_tool_provider::errors;

#[path = "remote_edit_paths.rs"]
mod edit_paths;

fn read_operation(wire_name: &str) -> Option<&'static str> {
    match crate::tool_gate::canonical_tool_name(wire_name)? {
        "read" => Some("read"),
        "grep" => Some("grep"),
        "glob" => Some("glob"),
        "aft_search" => Some("search"),
        "aft_outline" => Some("outline"),
        "aft_zoom" => Some("zoom"),
        "aft_callgraph" => Some("callgraph"),
        "ast_grep_search" => Some("ast_search"),
        // `conflicts` scans the entire Git working tree before checking file
        // paths, which could read outside the registered folder.
        _ => None,
    }
}

fn edit_operation(wire_name: &str) -> Option<&'static str> {
    match crate::tool_gate::canonical_tool_name(wire_name)? {
        "write" => Some("write"),
        "edit" => Some("edit"),
        "apply_patch" => Some("apply_patch"),
        "aft_delete" => Some("delete"),
        "aft_move" => Some("move"),
        "aft_import" => Some("import"),
        "ast_grep_replace" => Some("ast_replace"),
        "bash" => Some("bash"),
        _ => None,
    }
}

pub(super) fn operation(wire_name: &str) -> Option<(&'static str, Mode)> {
    read_operation(wire_name)
        .map(|op| (op, Mode::Read))
        .or_else(|| edit_operation(wire_name).map(|op| (op, Mode::ReadWrite)))
}

pub(super) fn requested_mode(wire_name: &str) -> Option<Mode> {
    operation(wire_name).map(|(_, mode)| mode).or_else(|| {
        match crate::tool_gate::canonical_tool_name(wire_name)? {
            "aft_conflicts" => Some(Mode::Read),
            _ => None,
        }
    })
}

pub(super) struct FileTool {
    operation: &'static str,
    mode: Mode,
    arguments: Value,
    write_paths: Vec<String>,
    context: ToolCallContext,
}

impl FileTool {
    pub(super) fn lane(&self) -> Lane {
        match self.mode {
            Mode::Read => Lane::PureRead,
            Mode::ReadWrite => Lane::Mutating,
        }
    }

    pub(super) fn prepare(
        call: ToolCallRequest,
        scope: ScopeStamp,
        folder: PathBuf,
        session: String,
        digest: String,
        ctx: &AppContext,
    ) -> Result<(Self, Preflight), ErrorBody> {
        let (operation, mode) = operation(&call.name).ok_or_else(tool_unavailable)?;
        // A hashline patch needs anchors from a prior read. Remote file tools
        // use fresh sessions and cannot share those anchors, so only standard
        // edits run here even if the caller pins the hashline edit schema.
        if operation == "edit" && call.arguments.get("patch").is_some() {
            return Err(tool_unavailable());
        }
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
        let tool = Self {
            operation,
            mode,
            arguments: call.arguments,
            write_paths: Vec::new(),
            context,
        };
        let _config = ctx.pin_config();
        let write_paths = confinement::with_remote_mutation(mode == Mode::ReadWrite, || {
            confinement::with_root(ctx, &tool.context.request_id, || {
                tool.check(ctx)?;
                let format = tool.format();
                let prepared = prepare_tool_call(
                    tool.operation,
                    tool.arguments.clone(),
                    &format,
                    &tool.context,
                    ctx,
                    None,
                )
                .map_err(|failed| failed.response)?;
                if tool.arguments.get("ref").is_some() {
                    // Git revision reads can open an ancestor or linked worktree's
                    // object store outside the registered folder.
                    return Err(crate::revision_read::disabled(&prepared.request));
                }
                if mode == Mode::ReadWrite {
                    let paths = edit_paths::operands(&prepared.request)?;
                    tool.check_writes(ctx, &paths)?;
                    return Ok(paths);
                }
                Ok(Vec::new())
            })
        })
        .map_err(|refusal| error(&refusal))?;
        let consent = Preflight {
            scope,
            session,
            folder,
            mode,
            tool: call.name,
            args_hash: digest,
        };
        Ok((
            Self {
                write_paths,
                ..tool
            },
            consent,
        ))
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
        confinement::preflight(ctx, id, session, &self.arguments)?;
        for operand in ["paths", "toPath", "destination"] {
            if let Some(paths) = self.arguments.get(operand) {
                confinement::preflight(ctx, id, session, &json!({"path": paths}))?;
            }
        }
        if self.operation == "grep" {
            if let Some(path) = self.arguments["path"].as_str() {
                let paths =
                    crate::subc_translate::split_search_path_arg(&self.context.project_root, path);
                confinement::preflight(ctx, id, session, &json!({"path": paths}))?;
            }
        }
        if let Some(batch) = self.arguments["batch"].as_array() {
            for item in batch {
                confinement::preflight(ctx, id, session, item)?;
            }
        }
        Ok(())
    }

    fn check_writes(&self, ctx: &AppContext, paths: &[String]) -> Result<(), Response> {
        let id = &self.context.request_id;
        let session = self
            .context
            .session_id
            .as_deref()
            .expect("remote file-tool session");
        edit_paths::check(ctx, id, session, paths)
    }

    pub(super) fn run(
        self,
        ctx: &AppContext,
        dispatch: DispatchFn,
    ) -> Result<ToolCallResult, ErrorBody> {
        let _config = ctx.pin_config();
        let format = self.format();
        let id = self.context.request_id.clone();
        let mutation = self.mode == Mode::ReadWrite;
        confinement::with_remote_mutation(mutation, || {
            confinement::with_root(ctx, &id, || {
                // Consent and scheduling may have delayed this call. Recheck the
                // current path and tool policy immediately before dispatch.
                self.check(ctx)?;
                self.check_writes(ctx, &self.write_paths)?;
                let dispatch = |mut request: RawRequest, ctx: &AppContext| {
                    if mutation {
                        // Keep normal file-change notifications, but never ask a
                        // language server to start for post-write diagnostics.
                        request.params["diagnostics"] = Value::Bool(false);
                    }
                    dispatch(request, ctx)
                };
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
        })
        .map_err(|refusal| error(&refusal))
    }
}

pub(super) fn error(refusal: &Response) -> ErrorBody {
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
