use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;
const GREP_FOOTER_FRESHNESS_WINDOW: Duration = Duration::from_secs(60);

use crate::bash_rewrite::footer::grep_search_hint;
use crate::bash_rewrite::notices::{self as notice_templates, Notice, Target};
use crate::bash_rewrite::parser::{parse, ParsedCommand};
use crate::bash_rewrite::{RewriteRequest, RewriteRule};
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub struct GrepRule;
pub struct RgRule;
pub struct FindRule;
pub struct CatRule;
pub struct HeadRule;
pub struct TailRule;
pub struct CatAppendRule;
pub struct SedRule;
pub struct LsRule;

// Dispatch shares one immutable parse across built-in rules. The public
// RewriteRule interface still parses standalone decisions for library callers.
pub(super) trait ParsedRewriteRule: RewriteRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision;
}

impl RewriteRule for GrepRule {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        let path = request.params.get("path").and_then(Value::as_str);
        let response = crate::bash_rewrite::dispatch::run(&tool_request("grep", request, ctx), ctx);
        grep_footer_response(response, request, ctx, path)
    }
}

impl ParsedRewriteRule for GrepRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = grep_request_from_parsed(parsed, "grep") else {
            return decline("grep", "grep.decline", "unsupported grep shape");
        };
        if let Some(path) = params.get("path").and_then(Value::as_str) {
            if !path_is_safe(ctx, path, true) {
                return decline(
                    "grep",
                    "grep.decline",
                    "grep path is outside the project root or missing",
                );
            }
            // Without -r, native grep refuses a directory operand.
            if !grep_flags_recursive(parsed) && path_candidate(ctx, path).is_dir() {
                return decline("grep", "grep.decline", "nonrecursive grep on a directory");
            }
        }
        accept(
            "grep",
            "grep.accept",
            "dc.grep.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for RgRule {
    fn name(&self) -> &'static str {
        "rg"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        let path = request.params.get("path").and_then(Value::as_str);
        let response = crate::bash_rewrite::dispatch::run(&tool_request("grep", request, ctx), ctx);
        grep_footer_response(response, request, ctx, path)
    }
}

impl ParsedRewriteRule for RgRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = grep_request_from_parsed(parsed, "rg") else {
            return decline("rg", "rg.decline", "unsupported rg shape");
        };
        if let Some(path) = params.get("path").and_then(Value::as_str) {
            if !path_is_safe(ctx, path, true) {
                return decline(
                    "rg",
                    "rg.decline",
                    "rg path is outside the project root or missing",
                );
            }
        }
        accept(
            "rg",
            "rg.accept",
            "dc.rg.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for FindRule {
    fn name(&self) -> &'static str {
        "find"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("glob", request, ctx), ctx),
            "glob",
            request,
        )
    }
}

impl ParsedRewriteRule for FindRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = find_request_from_parsed(parsed) else {
            return decline("find", "find.decline", "unsupported find shape");
        };
        if let Some(path) = params.get("path").and_then(Value::as_str) {
            if !path_is_safe(ctx, path, true) {
                return decline(
                    "find",
                    "find.decline",
                    "find path is outside the project root or missing",
                );
            }
        }
        if !find_shape_is_faithful(ctx, &params) {
            return decline(
                "find",
                "find.decline",
                "find requires a complete files-only glob result",
            );
        }
        accept(
            "find",
            "find.accept",
            "dc.find.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for CatRule {
    fn name(&self) -> &'static str {
        "cat"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("read", request, ctx), ctx),
            "read",
            request,
        )
    }
}

impl ParsedRewriteRule for CatRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = cat_read_request_from_parsed(parsed) else {
            return decline("cat", "cat.decline", "unsupported cat shape");
        };
        let path = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !path_is_safe(ctx, path, true) {
            return decline_cat_read(ctx, path, "path_is_safe");
        }
        if !read_shape_is_faithful(ctx, path) {
            return decline_cat_read(ctx, path, "read_shape_is_faithful");
        }
        accept(
            "cat",
            "cat.accept",
            "dc.cat.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for HeadRule {
    fn name(&self) -> &'static str {
        "head"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("read", request, ctx), ctx),
            "read",
            request,
        )
    }
}

impl ParsedRewriteRule for HeadRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = head_tail_read_request_from_parsed(parsed, "head") else {
            return decline("head", "head.decline", "unsupported head shape");
        };
        let path = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let lines = params.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
        // Cheap eligibility first: the file is read only when a rewrite could
        // follow, and only once.
        let faithful = effective_hashline_session(ctx, session_id)
            && path_is_safe(ctx, path, true)
            && head_tail_text(ctx, path)
                .is_some_and(|text| head_tail_shape_is_faithful(&text, lines, false));
        if !faithful {
            return decline(
                "head",
                "head.decline",
                "head requires an effective hashline session and a faithful text-read shape",
            );
        }
        accept(
            "head",
            "head.accept",
            "dc.head.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for TailRule {
    fn name(&self) -> &'static str {
        "tail"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("read", request, ctx), ctx),
            "read",
            request,
        )
    }
}

impl ParsedRewriteRule for TailRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(mut params) = head_tail_read_request_from_parsed(parsed, "tail") else {
            return decline("tail", "tail.decline", "unsupported tail shape");
        };
        let path = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        const REQUIREMENT: &str =
            "tail requires an effective hashline session and a faithful text-read shape";
        // Cheap eligibility first: the file is read only when a rewrite could
        // follow, and only once.
        if !effective_hashline_session(ctx, session_id) || !path_is_safe(ctx, &path, true) {
            return decline("tail", "tail.decline", REQUIREMENT);
        }
        let text = head_tail_text(ctx, &path);
        if params.get("start_line").is_some()
            && !text
                .as_deref()
                .is_some_and(|text| resolve_tail_start_range(text, &mut params))
        {
            return decline(
                "tail",
                "tail.decline",
                "tail start line would produce an empty or unsupported read",
            );
        }
        let lines = params.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
        if !text
            .as_deref()
            .is_some_and(|text| head_tail_shape_is_faithful(text, lines, true))
        {
            return decline("tail", "tail.decline", REQUIREMENT);
        }
        accept(
            "tail",
            "tail.accept",
            "dc.tail.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for CatAppendRule {
    fn name(&self) -> &'static str {
        "cat_append"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("edit_match", request, ctx), ctx),
            "edit",
            request,
        )
    }
}

impl ParsedRewriteRule for CatAppendRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = append_request_from_parsed(parsed) else {
            return decline(
                "cat_append",
                "cat_append.decline",
                "unsupported append shape",
            );
        };
        let path = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !append_path_is_safe(ctx, path) {
            return decline(
                "cat_append",
                "cat_append.decline",
                "append path is outside the project root or has no existing parent",
            );
        }
        accept(
            "cat_append",
            "cat_append.accept",
            "dc.cat_append.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for SedRule {
    fn name(&self) -> &'static str {
        "sed"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("read", request, ctx), ctx),
            "read",
            request,
        )
    }
}

impl ParsedRewriteRule for SedRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = sed_request_from_parsed(parsed) else {
            return decline("sed", "sed.decline", "unsupported sed shape");
        };
        let path = params
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !path_is_safe(ctx, path, true) || !read_shape_is_faithful(ctx, path) {
            return decline(
                "sed",
                "sed.decline",
                "sed path is outside the project root or exceeds the read contract",
            );
        }
        accept(
            "sed",
            "sed.accept",
            "dc.sed.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

impl RewriteRule for LsRule {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let parsed = parse(command);
        self.decide_parsed(parsed.as_ref(), command, request_id, session_id, ctx)
    }

    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response {
        call_and_footer(
            crate::bash_rewrite::dispatch::run(&tool_request("read", request, ctx), ctx),
            "read",
            request,
        )
    }
}

impl ParsedRewriteRule for LsRule {
    fn decide_parsed(
        &self,
        parsed: Option<&ParsedCommand>,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> crate::bash_rewrite::RewriteDecision {
        let Some(params) = ls_request_from_parsed(parsed, ctx) else {
            return decline("ls", "ls.decline", "unsupported ls shape or target");
        };
        accept(
            "ls",
            "ls.accept",
            "dc.ls.accept.v1",
            command,
            request_id,
            session_id,
            params,
        )
    }
}

fn accept(
    rule_id: &'static str,
    branch_id: &'static str,
    decision_class_id: &'static str,
    command: &str,
    request_id: &str,
    session_id: Option<&str>,
    params: Value,
) -> crate::bash_rewrite::RewriteDecision {
    crate::bash_rewrite::RewriteDecision::Accept(RewriteRequest {
        request_id: request_id.to_string(),
        command: command.to_string(),
        session_id: session_id.map(str::to_owned),
        rule_id,
        branch_id,
        decision_class_id,
        params,
        host_tool_names: crate::host_tool_names::HostToolNames::default(),
    })
}

fn decline(
    rule_id: &'static str,
    branch_id: &'static str,
    reason: &str,
) -> crate::bash_rewrite::RewriteDecision {
    let decision_class_id = match rule_id {
        "grep" => "dc.grep.decline.v1",
        "rg" => "dc.rg.decline.v1",
        "find" => "dc.find.decline.v1",
        "cat" => "dc.cat.decline.v1",
        "head" => "dc.head.decline.v1",
        "tail" => "dc.tail.decline.v1",
        "cat_append" => "dc.cat_append.decline.v1",
        "sed" => "dc.sed.decline.v1",
        "ls" => "dc.ls.decline.v1",
        _ => "dc.native.decline.v1",
    };
    crate::bash_rewrite::RewriteDecision::Decline(crate::bash_rewrite::DeclineReason {
        rule_id: Some(rule_id),
        branch_id,
        decision_class_id,
        reason: reason.to_string(),
    })
}

fn tool_request(tool: &str, request: &RewriteRequest, ctx: &AppContext) -> RawRequest {
    let mut params = request.params.clone();
    let root = grep_project_root(ctx);
    if matches!(tool, "read" | "edit_match") {
        if let Some(file) = params
            .get("file")
            .and_then(Value::as_str)
            .map(str::to_owned)
        {
            if tool == "read" && matches!(request.rule_id, "cat" | "head" | "tail") {
                params["_hashline_requested_path"] = Value::String(file.clone());
                params["_hashline_bash_read_kind"] = Value::String(request.rule_id.to_string());
                if let Some(lines) = request.params.get("limit").and_then(Value::as_u64) {
                    params["_hashline_bash_read_lines"] = Value::from(lines);
                }
            }
            let path = Path::new(&file);
            if path.is_relative() {
                params["file"] = Value::String(root.join(path).display().to_string());
            }
            if tool == "read" && request.rule_id == "tail" {
                if let Some(path) = params.get("file").and_then(Value::as_str) {
                    if let Some(total_lines) = text_file_line_count(Path::new(path)) {
                        if total_lines > 0 {
                            let lines = request
                                .params
                                .get("limit")
                                .and_then(Value::as_u64)
                                .unwrap_or(10) as usize;
                            params["start_line"] =
                                Value::from(total_lines.saturating_sub(lines).saturating_add(1));
                            params["end_line"] = Value::from(total_lines);
                        }
                    }
                }
            }
        }
    }
    if tool == "glob" && params.get("path").is_none() {
        params["path"] = Value::String(root.display().to_string());
    }
    RawRequest {
        id: request.request_id.clone(),
        command: tool.to_string(),
        lsp_hints: None,
        session_id: request.session_id.clone(),
        params,
    }
}

/// Add the normal tool footer while preserving a handler error as the final
/// response. A handler error is not a permission to execute native bash: the
/// request has already entered the internal handler.
fn call_and_footer(
    mut response: Response,
    replacement_tool: &str,
    request: &RewriteRequest,
) -> Response {
    let output = response_output(&response.data);
    let shell = request.host_tool_names.shell();
    response.notices.items.push(Notice::template(
        notice_templates::PREFER_TOOL,
        json!({"tool": replacement_tool, "shell": shell}),
        format!("Prefer `{replacement_tool}` tool over {shell}."),
        Target::RewriteFooter,
    ));
    apply_footer(response, output)
}

fn grep_footer_response(
    mut response: Response,
    request: &RewriteRequest,
    ctx: &AppContext,
    path: Option<&str>,
) -> Response {
    let output = response_output(&response.data);
    if !should_suppress_grep_footer(path, &grep_project_root(ctx)) {
        let search = ctx.config().aft_search_registered.then_some("aft_search");
        let shell = request.host_tool_names.shell();
        response.notices.items.push(Notice::template(
            notice_templates::CODE_SEARCH,
            notice_templates::code_search_slots(search, shell),
            grep_search_hint(search, "grep", shell),
            Target::RewriteFooter,
        ));
    }
    apply_footer(response, output)
}

pub(crate) fn append_decided_footer(response: &mut Response, footer: &str) {
    let output = format!("{}\n\n{footer}", response_output(&response.data));
    if let Some(object) = response.data.as_object_mut() {
        object.insert("output".into(), json!(output));
        for key in ["text", "content", "message"] {
            if object.get(key).is_some_and(Value::is_string) {
                object.insert(key.into(), json!(output));
                break;
            }
        }
    }
}

fn grep_project_root(ctx: &AppContext) -> std::path::PathBuf {
    let configured = ctx
        .config()
        .project_root
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    std::fs::canonicalize(&configured).unwrap_or(configured)
}

pub(crate) fn should_suppress_grep_footer(path: Option<&str>, project_root: &Path) -> bool {
    let Some(path) = path else {
        return false;
    };
    // Canonicalize the root here rather than trusting callers: the target
    // below is canonicalized, and comparing a canonical target against a
    // non-canonical root breaks on alias spellings (macOS /var vs
    // /private/var), misreading in-root paths as external.
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let project_root = project_root.as_path();
    let target = Path::new(path);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        project_root.join(target)
    };
    let Ok(target) = std::fs::canonicalize(target) else {
        return false;
    };
    if !target.starts_with(project_root) {
        return true;
    }
    let Ok(metadata) = std::fs::metadata(&target) else {
        return false;
    };
    if metadata.is_file() {
        return true;
    }
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age < GREP_FOOTER_FRESHNESS_WINDOW)
}

fn apply_footer(mut response: Response, output: String) -> Response {
    if let Some(object) = response.data.as_object_mut() {
        object.insert("output".to_string(), Value::String(output.clone()));

        for key in ["text", "content", "message"] {
            if object.get(key).is_some_and(Value::is_string) {
                object.insert(key.to_string(), Value::String(output.clone()));
                break;
            }
        }
    } else {
        response.data = json!({ "output": output });
    }

    response
}

fn response_output(data: &Value) -> String {
    if let Some(output) = data.get("output").and_then(Value::as_str) {
        return output.to_string();
    }
    if let Some(text) = data.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(content) = data.get("content").and_then(Value::as_str) {
        return content.to_string();
    }
    if let Some(message) = data.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    if let Some(entries) = data.get("entries").and_then(Value::as_array) {
        return entries
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
    }
    serde_json::to_string_pretty(data).unwrap_or_else(|_| data.to_string())
}

fn path_candidate(ctx: &AppContext, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        grep_project_root(ctx).join(candidate)
    }
}

fn resolved_path_candidate(ctx: &AppContext, path: &str) -> PathBuf {
    let candidate = path_candidate(ctx, path);
    std::fs::canonicalize(&candidate).unwrap_or(candidate)
}

fn decline_cat_read(
    ctx: &AppContext,
    path: &str,
    predicate: &'static str,
) -> crate::bash_rewrite::RewriteDecision {
    let candidate = resolved_path_candidate(ctx, path);
    // Routine: a cat outside the project root (a host temp folder, say) or of
    // an unreadable shape simply runs as native bash.
    crate::slog_debug!(
        "bash rewrite rule cat declined: read declined: predicate={} resolved_candidate={:?}",
        predicate,
        candidate
    );
    decline(
        "cat",
        "cat.decline",
        &format!(
            "read declined: {predicate} failed for {}",
            candidate.display()
        ),
    )
}

fn path_is_safe(ctx: &AppContext, path: &str, require_existing: bool) -> bool {
    let root = grep_project_root(ctx);
    let candidate = path_candidate(ctx, path);
    if require_existing && !candidate.exists() {
        return false;
    }
    let resolved = std::fs::canonicalize(&candidate).unwrap_or(candidate);
    resolved.starts_with(&root)
}

fn read_shape_is_faithful(ctx: &AppContext, path: &str) -> bool {
    let candidate = path_candidate(ctx, path);
    let Ok(metadata) = std::fs::metadata(&candidate) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() > 50 * 1024 {
        return false;
    }
    let Ok(bytes) = std::fs::read(candidate) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    text.lines().all(|line| line.len() <= 2_000)
}

fn effective_hashline_session(ctx: &AppContext, session_id: Option<&str>) -> bool {
    let root = ctx
        .canonical_cache_root_opt()
        .unwrap_or_else(|| grep_project_root(ctx));
    ctx.hashline_bindings()
        .capture(
            root,
            session_id.unwrap_or(crate::protocol::DEFAULT_SESSION_ID),
        )
        .is_some_and(|binding| binding.effective())
}

/// Largest file a head/tail rewrite decision reads. Larger files stay with
/// the native command, which reads only the lines it prints.
const HEAD_TAIL_REWRITE_MAX_BYTES: u64 = 1024 * 1024;

/// The whole text of a regular UTF-8 file within the rewrite budget, read once
/// per decision. The read itself stops past the budget, so a file that grows
/// after its size check is not loaded in full.
fn head_tail_text(ctx: &AppContext, path: &str) -> Option<String> {
    use std::io::Read;

    let candidate = path_candidate(ctx, path);
    let metadata = std::fs::metadata(&candidate).ok()?;
    if !metadata.is_file() || metadata.len() > HEAD_TAIL_REWRITE_MAX_BYTES {
        return None;
    }
    record_head_tail_preflight_read();
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    std::fs::File::open(candidate)
        .ok()?
        .take(HEAD_TAIL_REWRITE_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > HEAD_TAIL_REWRITE_MAX_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn head_tail_shape_is_faithful(text: &str, lines: usize, tail: bool) -> bool {
    let skip = if tail {
        text.lines().count().saturating_sub(lines)
    } else {
        0
    };
    let mut displayed_bytes = 0_usize;
    for record in text.lines().skip(skip).take(lines) {
        if record.len() > 2_000 {
            return false;
        }
        displayed_bytes = displayed_bytes
            .saturating_add(record.len())
            .saturating_add(16);
        if displayed_bytes > 50 * 1024 {
            return false;
        }
    }
    true
}

fn text_file_line_count(path: &Path) -> Option<usize> {
    record_head_tail_preflight_read();
    let bytes = std::fs::read(path).ok()?;
    std::str::from_utf8(&bytes).ok()?;
    Some(text_line_count(&bytes))
}

fn text_line_count(bytes: &[u8]) -> usize {
    if bytes.is_empty() {
        return 0;
    }
    bytes.iter().filter(|byte| **byte == b'\n').count() + usize::from(bytes.last() != Some(&b'\n'))
}

fn resolve_tail_start_range(text: &str, params: &mut Value) -> bool {
    let Some(requested_start) = params.get("start_line").and_then(Value::as_u64) else {
        return false;
    };
    let Ok(requested_start) = usize::try_from(requested_start) else {
        return false;
    };
    let start_line = requested_start.max(1);
    let total_lines = text_line_count(text.as_bytes());
    if total_lines == 0 || start_line > total_lines {
        return false;
    }

    params["start_line"] = Value::from(start_line);
    params["end_line"] = Value::from(total_lines);
    params["limit"] = Value::from(total_lines - start_line + 1);
    true
}

fn append_path_is_safe(ctx: &AppContext, path: &str) -> bool {
    let root = grep_project_root(ctx);
    let candidate = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        root.join(path)
    };
    let parent = candidate.parent().unwrap_or(&root);
    if !parent.exists() {
        return false;
    }
    let resolved_parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    if !resolved_parent.starts_with(&root) {
        return false;
    }
    if candidate.exists() {
        let Ok(resolved) = std::fs::canonicalize(&candidate) else {
            return false;
        };
        resolved.starts_with(&root)
    } else {
        true
    }
}

pub(crate) fn grep_basic_regex_has_dialect_conflict(pattern: &str) -> bool {
    // Basic grep treats these characters as literals unless escaped, while the
    // AFT regex engine gives them extended-regex meanings. BRE also treats `^`
    // and `$` as anchors only at the beginning and end of the whole pattern.
    // Native grep is the only faithful route until the rewrite can translate
    // the complete BRE.
    let last_index = pattern.chars().count().saturating_sub(1);
    pattern.chars().enumerate().any(|(index, ch)| {
        matches!(ch, '+' | '?' | '|' | '(' | ')' | '{' | '}')
            || (ch == '^' && index != 0)
            || (ch == '$' && index != last_index)
    })
}

/// Whether the option words before the pattern include `-r`. The shape parser
/// accepts only bundled single-letter flags, so `r` in one means recursion.
fn grep_flags_recursive(parsed: Option<&ParsedCommand>) -> bool {
    parsed.is_some_and(|parsed| {
        parsed
            .args
            .iter()
            .skip(1)
            .take_while(|arg| arg.starts_with('-') && arg.as_str() != "-")
            .any(|arg| arg[1..].contains('r'))
    })
}

fn grep_request_from_parsed(parsed: Option<&ParsedCommand>, binary: &str) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() || parsed.args.first()? != binary {
        return None;
    }

    let mut case_sensitive = true;
    let mut word_match = false;
    let mut index = 1;

    while let Some(arg) = parsed.args.get(index) {
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        for flag in arg[1..].chars() {
            match flag {
                'n' | 'r' => {}
                'i' => case_sensitive = false,
                'w' => word_match = true,
                _ => return None,
            }
        }
        index += 1;
    }

    let pattern = parsed.args.get(index)?.clone();
    if binary == "grep" && grep_basic_regex_has_dialect_conflict(&pattern) {
        return None;
    }
    let path = parsed.args.get(index + 1).cloned();
    if parsed.args.len() > index + 2 {
        return None;
    }
    // Native grep with no path and no -r reads stdin, not the project.
    if binary == "grep" && path.is_none() && !grep_flags_recursive(Some(parsed)) {
        return None;
    }

    let pattern = if word_match {
        format!(r"\b(?:{})\b", pattern)
    } else {
        pattern
    };

    if regex::RegexBuilder::new(&pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .is_err()
    {
        return None;
    }

    let mut params = json!({
        "pattern": pattern,
        "case_sensitive": case_sensitive,
    });
    if let Some(path) = path {
        params["path"] = json!(path);
    }
    Some(params)
}

fn find_shape_is_faithful(ctx: &AppContext, params: &Value) -> bool {
    let Some(pattern) = params.get("pattern").and_then(Value::as_str) else {
        return false;
    };
    let Some(name) = pattern.strip_prefix("**/") else {
        return false;
    };
    if name.is_empty()
        || name
            .chars()
            .any(|ch| matches!(ch, '/' | '\\' | '?' | '[' | ']' | '{' | '}'))
    {
        return false;
    }
    let Ok(name_pattern) = glob::Pattern::new(name) else {
        return false;
    };

    let project_root = grep_project_root(ctx);
    let search_root = params
        .get("path")
        .and_then(Value::as_str)
        .map(|path| path_candidate(ctx, path))
        .unwrap_or_else(|| project_root.clone());
    let search_root = std::fs::canonicalize(search_root).ok();
    let Some(search_root) = search_root.filter(|path| path.is_dir()) else {
        return false;
    };
    let relative_root = search_root.strip_prefix(&project_root).ok();
    if relative_root.is_some_and(path_has_glob_skipped_directory)
        || ctx.gitignore().is_some_and(|matcher| {
            matcher
                .matched_path_or_any_parents(&search_root, true)
                .is_ignore()
        })
    {
        return false;
    }

    // The glob backend can omit ignored, unindexed, hard-skipped, or capped
    // files. Compare its complete result with a direct files-only walk and only
    // rewrite when those candidate sets are identical.
    let Some(native_candidates) = find_matching_files(&search_root, &name_pattern) else {
        return false;
    };
    let mut glob_params = params.clone();
    if glob_params.get("path").is_none() {
        glob_params["path"] = Value::String(project_root.display().to_string());
    }
    let response = crate::bash_rewrite::dispatch::run(
        &RawRequest {
            id: "bash-rewrite-find-faithfulness".to_string(),
            command: "glob".to_string(),
            lsp_hints: None,
            session_id: None,
            params: glob_params,
        },
        ctx,
    );
    if !response.success
        || response.data.get("complete").and_then(Value::as_bool) != Some(true)
        || response.data.get("truncated").and_then(Value::as_bool) != Some(false)
    {
        return false;
    }
    let Some(glob_files) = response.data.get("files").and_then(Value::as_array) else {
        return false;
    };
    let glob_candidates = glob_files
        .iter()
        .map(|value| value.as_str().map(PathBuf::from))
        .collect::<Option<Vec<_>>>()
        .and_then(|paths| {
            paths
                .into_iter()
                .map(std::fs::canonicalize)
                .collect::<Result<BTreeSet<_>, _>>()
                .ok()
        });
    glob_candidates.as_ref() == Some(&native_candidates)
}

fn path_has_glob_skipped_directory(path: &Path) -> bool {
    // These directory names are pruned by grep_executor's fallback glob walk.
    // If the requested root is below one of them, the glob would skip files
    // that native find inspects, so the rewrite must decline.
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            "node_modules"
                | "target"
                | "venv"
                | ".venv"
                | ".git"
                | "__pycache__"
                | ".tox"
                | "dist"
                | "build"
        )
    })
}

fn find_matching_files(root: &Path, pattern: &glob::Pattern) -> Option<BTreeSet<PathBuf>> {
    // A large direct walk would erase the optimization's benefit. Declining at
    // the bound is safe because native find remains the source of truth.
    const MAX_ENTRIES: usize = 10_000;

    fn visit(
        directory: &Path,
        pattern: &glob::Pattern,
        entries_seen: &mut usize,
        matches: &mut BTreeSet<PathBuf>,
    ) -> Option<()> {
        for entry in std::fs::read_dir(directory).ok()? {
            let entry = entry.ok()?;
            *entries_seen = entries_seen.checked_add(1)?;
            if *entries_seen > MAX_ENTRIES {
                return None;
            }
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let file_type = entry.file_type().ok()?;
            if file_type.is_dir() {
                visit(&entry.path(), pattern, entries_seen, matches)?;
            } else if file_type.is_file() && pattern.matches(file_name) {
                matches.insert(std::fs::canonicalize(entry.path()).ok()?);
            }
        }
        Some(())
    }

    let mut entries_seen = 0;
    let mut matches = BTreeSet::new();
    visit(root, pattern, &mut entries_seen, &mut matches)?;
    Some(matches)
}

fn find_request_from_parsed(parsed: Option<&ParsedCommand>) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() || parsed.args.first()? != "find" {
        return None;
    }
    if parsed.args.len() != 4 && parsed.args.len() != 6 {
        return None;
    }

    let path = parsed.args.get(1)?.clone();
    let mut name = None;
    let mut saw_type_file = false;
    let mut index = 2;

    while index < parsed.args.len() {
        match parsed.args[index].as_str() {
            "-name" if name.is_none() && index + 1 < parsed.args.len() => {
                name = Some(parsed.args[index + 1].clone());
                index += 2;
            }
            "-type" if !saw_type_file && index + 1 < parsed.args.len() => {
                if parsed.args[index + 1] != "f" {
                    return None;
                }
                saw_type_file = true;
                index += 2;
            }
            _ => return None,
        }
    }

    let name = name?;
    if !saw_type_file {
        return None;
    }
    let pattern = format!("**/{name}");
    if path == "." {
        Some(json!({ "pattern": pattern }))
    } else {
        let trimmed = path.trim_end_matches('/');
        if trimmed.is_empty() {
            // Filesystem root (`find / ...`, `find // ...`): trimming the slash
            // yields "" which downstream resolves as the PROJECT ROOT — silently
            // searching the project instead of the whole filesystem. Don't
            // rewrite; fall through to native `find`, which does what was asked.
            // (A non-empty absolute path like `/tmp/foo` is preserved as-is.)
            None
        } else {
            Some(json!({ "path": trimmed, "pattern": pattern }))
        }
    }
}

fn cat_read_request_from_parsed(parsed: Option<&ParsedCommand>) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() {
        return None;
    }
    if parsed.args.len() != 2 || parsed.args.first()? != "cat" {
        return None;
    }
    Some(json!({ "file": parsed.args[1] }))
}

fn head_tail_read_request_from_parsed(
    parsed: Option<&ParsedCommand>,
    command_name: &str,
) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some()
        || parsed.heredoc.is_some()
        || parsed.args.first()? != command_name
    {
        return None;
    }

    let (lines, file) = match parsed.args.as_slice() {
        [_command, file] => (10_u64, file.as_str()),
        [_command, flag, count, file] if flag == "-n" => {
            if command_name == "tail" {
                if let Some(start_line) = count.strip_prefix('+') {
                    let start_line = start_line.parse::<u64>().ok()?;
                    return Some(json!({ "file": file, "start_line": start_line }));
                }
            }
            (count.parse::<u64>().ok()?, file.as_str())
        }
        [_command, compact, file] => {
            let count = compact.strip_prefix('-')?.parse::<u64>().ok()?;
            (count, file.as_str())
        }
        _ => return None,
    };
    if lines == 0 {
        return None;
    }
    Some(json!({ "file": file, "limit": lines }))
}

fn append_request_from_parsed(parsed: Option<&ParsedCommand>) -> Option<Value> {
    let parsed = parsed?;
    let file = parsed.appends_to.clone()?;

    let append_content = if parsed.args == ["cat"] {
        parsed.heredoc.clone()?
    } else if parsed.heredoc.is_none()
        && parsed.args.first().is_some_and(|arg| arg == "echo")
        && parsed.args.len() >= 2
        && !parsed.args[1].starts_with('-')
    {
        format!("{}\n", parsed.args[1..].join(" "))
    } else {
        return None;
    };

    Some(json!({
        "op": "append",
        "file": file,
        "append_content": append_content,
        "create_dirs": true,
    }))
}

fn sed_request_from_parsed(parsed: Option<&ParsedCommand>) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() {
        return None;
    }
    if parsed.args.len() != 4 || parsed.args.first()? != "sed" || parsed.args[1] != "-n" {
        return None;
    }

    let range = parsed.args[2].strip_suffix('p')?;
    let (start, end) = range.split_once(',')?;
    let start_line = start.parse::<u32>().ok()?;
    let end_line = end.parse::<u32>().ok()?;
    if start_line == 0 || end_line < start_line {
        return None;
    }

    Some(json!({
        "file": parsed.args[3],
        "start_line": start_line,
        "end_line": end_line,
    }))
}

fn ls_request_from_parsed(parsed: Option<&ParsedCommand>, ctx: &AppContext) -> Option<Value> {
    let parsed = parsed?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() || parsed.args.first()? != "ls" {
        return None;
    }

    let mut path = None;
    let mut include_hidden = false;
    for arg in parsed.args.iter().skip(1) {
        if let Some(flags) = arg.strip_prefix('-') {
            if flags.is_empty() {
                return None;
            }
            for flag in flags.chars() {
                match flag {
                    // Directory reads are single-level; recursive listing must
                    // stay with the shell to preserve all descendants.
                    'R' => return None,
                    // `read` lists a directory without `.` and `..`, which is
                    // exactly the `ls -A` contract (hidden entries included).
                    'A' => include_hidden = true,
                    // `ls -a` also prints `.` and `..`, which a directory read
                    // never returns, so it stays with the shell.
                    'a' => return None,
                    // -l: long format. Shows size, mtime, permissions, owner.
                    // `read` returns directory entries (no metadata) or file
                    // contents (not metadata at all). Rewriting drops the
                    // info the user asked for, so fall through to real bash.
                    // Reported by user dogfooding the v0.18 bash experimentals.
                    _ => return None,
                }
            }
        } else if path.is_none() {
            path = Some(arg.clone());
        } else {
            return None;
        }
    }

    // Even without -l, `ls FILE` and `read FILE` have entirely different
    // semantics: `ls FILE` echoes the filename, `read FILE` dumps the file
    // contents. The rewrite is only safe when the path resolves to a
    // directory (or is missing/cwd, where `read` of cwd also makes sense).
    // Stat the path and fall through to bash for files.
    let target = path.clone().unwrap_or_else(|| ".".to_string());
    let root = grep_project_root(ctx);
    let target_for_metadata = if Path::new(&target).is_absolute() {
        Path::new(&target).to_path_buf()
    } else {
        root.join(&target)
    };
    if let Ok(metadata) = std::fs::metadata(&target_for_metadata) {
        if !metadata.is_dir() || !path_is_safe(ctx, &target, true) {
            return None;
        }
    } else {
        // Path doesn't exist (yet)? Let bash handle the error itself — its
        // wording is well-known to agents, and rewriting a guaranteed failure
        // would change the native error outcome.
        return None;
    }

    Some(json!({ "file": target, "include_hidden": include_hidden }))
}

#[cfg(test)]
pub(crate) fn grep_request(command: &str, binary: &str) -> Option<Value> {
    grep_request_from_parsed(parse(command).as_ref(), binary)
}

#[cfg(test)]
thread_local! {
    static HEAD_TAIL_PREFLIGHT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts whole-file reads made while deciding a head/tail rewrite.
fn record_head_tail_preflight_read() {
    #[cfg(test)]
    HEAD_TAIL_PREFLIGHT_READS.with(|reads| reads.set(reads.get() + 1));
}

#[cfg(test)]
fn reset_head_tail_preflight_reads() {
    HEAD_TAIL_PREFLIGHT_READS.with(|reads| reads.set(0));
}

#[cfg(test)]
fn head_tail_preflight_reads() -> usize {
    HEAD_TAIL_PREFLIGHT_READS.with(std::cell::Cell::get)
}

#[cfg(test)]
pub(crate) fn find_request(command: &str) -> Option<Value> {
    find_request_from_parsed(parse(command).as_ref())
}

#[cfg(test)]
pub(crate) fn ls_request(command: &str, ctx: &AppContext) -> Option<Value> {
    ls_request_from_parsed(parse(command).as_ref(), ctx)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    use super::{
        find_request, grep_basic_regex_has_dialect_conflict, grep_request,
        should_suppress_grep_footer, HeadRule, TailRule,
    };
    use crate::bash_rewrite::{RewriteDecision, RewriteRule};
    use crate::config::Config;
    use crate::context::{default_language_provider_factory, AppContext};
    use crate::hashline::integration::RegistrationRequest;
    use crate::protocol::DEFAULT_SESSION_ID;

    #[test]
    fn recursive_ls_declines_single_level_rewrite() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        for command in ["ls -R", "ls -AR", "ls -RA ."] {
            assert!(super::ls_request(command, &ctx).is_none(), "{command}");
        }
        assert!(super::ls_request("ls -A", &ctx).is_some());
    }

    /// A directory read never lists `.` and `..`, which is the `ls -A`
    /// contract. `ls -a` prints them too, so only `-A` may be rewritten.
    #[test]
    fn ls_rewrites_dash_capital_a_and_keeps_dash_a_native() {
        let root = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let request = super::ls_request("ls -A", &ctx).expect("ls -A is rewritten");
        assert_eq!(request["include_hidden"], json!(true));
        for command in ["ls -a", "ls -la", "ls -aA", "ls -Aa ."] {
            assert!(super::ls_request(command, &ctx).is_none(), "{command}");
        }
    }

    // The oracle needs a real POSIX bash. On Windows `bash` on PATH is often the
    // WSL launcher, which fails without an installed distribution, so the byte
    // comparison runs on Unix runners only.
    #[cfg(unix)]
    #[test]
    fn literal_append_rewrites_match_bash_bytes() {
        use crate::bash_rewrite::parser::tests::LITERAL_APPEND_CASES;
        use std::process::Command;

        if which::which("bash").is_err() {
            eprintln!("skipping bash byte oracle: bash is unavailable");
            return;
        }
        let native = tempfile::tempdir().unwrap();
        let rewritten = tempfile::tempdir().unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(rewritten.path().to_path_buf()),
                experimental_bash_rewrite: true,
                restrict_to_project_root: true,
                ..Config::default()
            },
        );
        for command in LITERAL_APPEND_CASES {
            fs::write(native.path().join("notes.txt"), "existing\n").unwrap();
            fs::write(rewritten.path().join("notes.txt"), "existing\n").unwrap();
            let output = Command::new("bash")
                .args(["--noprofile", "--norc", "-c", command])
                .env_remove("BASH_ENV")
                .current_dir(native.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "bash failed: {command}: {output:?}"
            );
            let RewriteDecision::Accept(request) =
                super::CatAppendRule.decide(command, "literal-oracle", None, &ctx)
            else {
                panic!("must rewrite: {command}");
            };
            let response = super::CatAppendRule.execute(&request, &ctx);
            assert!(
                response.success || response.data["code"] == "no_change",
                "rewrite failed: {command}: {response:?}"
            );
            assert_eq!(
                fs::read(rewritten.path().join("notes.txt")).unwrap(),
                fs::read(native.path().join("notes.txt")).unwrap(),
                "byte mismatch: {command}"
            );
        }
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/app.ts"), "foo\n").unwrap();
        dir
    }

    #[test]
    fn single_named_file_suppresses_grep_footer() {
        let dir = fixture();
        assert!(should_suppress_grep_footer(Some("src/app.ts"), dir.path()));
    }

    #[test]
    fn directory_path_keeps_grep_footer() {
        let dir = fixture();
        filetime::set_file_mtime(
            dir.path().join("src"),
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(!should_suppress_grep_footer(Some("src"), dir.path()));
    }

    #[test]
    fn no_path_keeps_grep_footer() {
        let dir = fixture();
        assert!(!should_suppress_grep_footer(None, dir.path()));
    }

    #[test]
    fn external_file_suppresses_grep_footer() {
        let dir = fixture();
        let external = tempfile::NamedTempFile::new().unwrap();
        assert!(should_suppress_grep_footer(
            external.path().to_str(),
            dir.path()
        ));
    }

    #[test]
    fn freshly_modified_file_suppresses_grep_footer() {
        let dir = fixture();
        let file = dir.path().join("src/app.ts");
        fs::write(&file, "foo\nbar\n").unwrap();
        assert!(should_suppress_grep_footer(file.to_str(), dir.path()));
    }

    #[test]
    fn old_directory_inside_project_root_keeps_grep_footer() {
        let dir = fixture();
        let directory = dir.path().join("src");
        filetime::set_file_mtime(
            &directory,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(!should_suppress_grep_footer(Some("src"), dir.path()));
    }

    #[test]
    fn old_file_inside_project_root_suppresses_grep_footer() {
        let dir = fixture();
        let file = dir.path().join("src/app.ts");
        filetime::set_file_mtime(
            &file,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
        assert!(should_suppress_grep_footer(Some("src/app.ts"), dir.path()));
    }

    #[test]
    fn grep_basic_regex_anchor_positions_and_escaped_operators_are_guarded() {
        for pattern in ["$HOME", "a$b", "x^y"] {
            assert!(grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
        for pattern in ["foo$", "^fn "] {
            assert!(!grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
        for pattern in [r"\(a\)", r"\{1\}", r"a\|b"] {
            assert!(grep_basic_regex_has_dialect_conflict(pattern), "{pattern}");
        }
    }

    #[test]
    fn grep_declines_basic_regex_tokens_with_extended_meanings() {
        assert_eq!(grep_request("grep -rn 'a+b' src", "grep"), None);
        assert_eq!(
            grep_request("rg -n 'a+b' src", "rg").unwrap()["pattern"],
            "a+b"
        );
        assert_eq!(
            grep_request("grep -rn 'a.b' src", "grep").unwrap()["pattern"],
            "a.b"
        );
    }

    /// Native grep without -r reads stdin when given no path and refuses a
    /// directory, so neither form may become a project search. With -r both
    /// search the tree, and rg keeps its own default of the working directory.
    #[test]
    fn grep_keeps_stdin_and_nonrecursive_directory_forms_native() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("match.txt"), "needle\n").unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.path().to_path_buf()),
                ..Config::default()
            },
        );
        let decide = |command: &str| super::GrepRule.decide(command, "grep-shape", None, &ctx);
        for command in [
            "grep needle",
            "grep -n needle",
            "grep needle .",
            "grep -in needle .",
        ] {
            assert!(
                !matches!(decide(command), RewriteDecision::Accept(_)),
                "{command} must stay native"
            );
        }
        for command in [
            "grep -r needle",
            "grep -r needle .",
            "grep -rn needle .",
            "grep needle match.txt",
        ] {
            assert!(
                matches!(decide(command), RewriteDecision::Accept(_)),
                "{command} must be rewritten"
            );
        }
        assert!(grep_request("rg needle", "rg").is_some());
    }

    #[test]
    fn find_absolute_path_uses_glob_path_arg() {
        assert_eq!(
            find_request(r#"find /tmp/foo -name "*.ts" -type f"#),
            Some(json!({ "path": "/tmp/foo", "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_dot_keeps_project_root_relative_pattern() {
        assert_eq!(
            find_request(r#"find . -name "*.ts" -type f"#),
            Some(json!({ "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_relative_path_uses_glob_path_arg() {
        assert_eq!(
            find_request(r#"find ./src -name "*.go" -type f"#),
            Some(json!({ "path": "./src", "pattern": "**/*.go" }))
        );
    }

    #[test]
    fn find_trims_trailing_slash_from_path_arg() {
        assert_eq!(
            find_request(r#"find /tmp/foo/ -type f -name "*.ts""#),
            Some(json!({ "path": "/tmp/foo", "pattern": "**/*.ts" }))
        );
    }

    #[test]
    fn find_requires_explicit_file_type() {
        assert_eq!(find_request(r#"find . -name sub"#), None);
    }

    #[test]
    fn find_filesystem_root_is_not_rewritten() {
        // `find /` must NOT rewrite — trimming the slash would yield "" which
        // resolves as the project root, silently searching the wrong scope.
        assert_eq!(find_request(r#"find / -name "*.rs" -type f"#), None);
        assert_eq!(find_request(r#"find // -type f -name "*.rs""#), None);
    }

    #[test]
    fn head_and_tail_rewrites_are_hashline_only_and_tail_keeps_absolute_numbering() {
        let dir = fixture();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join("src/app.ts"), "one\ntwo\nthree\nfour\n").unwrap();
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                experimental_bash_rewrite: true,
                ..Default::default()
            },
        );

        assert!(matches!(
            HeadRule.decide("head -2 src/app.ts", "head-off", None, &ctx),
            RewriteDecision::Decline(_)
        ));
        assert!(matches!(
            TailRule.decide("tail -2 src/app.ts", "tail-off", None, &ctx),
            RewriteDecision::Decline(_)
        ));

        ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        let RewriteDecision::Accept(request) =
            TailRule.decide("tail -2 src/app.ts", "tail-on", None, &ctx)
        else {
            panic!("effective hashline tail should rewrite");
        };
        let response = TailRule.execute(&request, &ctx);
        assert!(response.success, "{}", response.data);
        let content = response.data["content"].as_str().unwrap();
        assert!(content.starts_with("[src/app.ts#"), "{content}");
        assert!(content.contains("3:three\n4:four\n"), "{content}");
    }

    /// Deciding a head/tail rewrite must not load a file larger than the
    /// preflight budget, and a session that cannot rewrite reads nothing.
    #[test]
    fn head_tail_decisions_skip_whole_file_reads_they_cannot_use() {
        let dir = fixture();
        let root = fs::canonicalize(dir.path()).unwrap();
        fs::write(root.join("src/app.ts"), "one\ntwo\nthree\nfour\n").unwrap();
        // A sparse file: large on disk metadata, cheap to create.
        let big = fs::File::create(root.join("src/big.log")).unwrap();
        big.set_len(64 * 1024 * 1024).unwrap();
        drop(big);
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                experimental_bash_rewrite: true,
                ..Default::default()
            },
        );

        // No hashline session: tail -n +N declines before any read.
        super::reset_head_tail_preflight_reads();
        assert!(matches!(
            TailRule.decide("tail -n +2 src/app.ts", "tail-off", None, &ctx),
            RewriteDecision::Decline(_)
        ));
        assert_eq!(super::head_tail_preflight_reads(), 0);

        ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        super::reset_head_tail_preflight_reads();
        assert!(matches!(
            HeadRule.decide("head -n 1 src/big.log", "big", None, &ctx),
            RewriteDecision::Decline(_)
        ));
        for command in ["tail -n 1 src/big.log", "tail -n +2 src/big.log"] {
            assert!(
                matches!(
                    TailRule.decide(command, "big", None, &ctx),
                    RewriteDecision::Decline(_)
                ),
                "{command}"
            );
        }
        assert_eq!(super::head_tail_preflight_reads(), 0);

        // A small file is read once to decide, even for tail -n +N.
        super::reset_head_tail_preflight_reads();
        assert!(matches!(
            TailRule.decide("tail -n +2 src/app.ts", "tail-on", None, &ctx),
            RewriteDecision::Accept(_)
        ));
        assert_eq!(super::head_tail_preflight_reads(), 1);
    }
}
