use serde_json::{json, Value};

const REGEX_SIZE_LIMIT: usize = 10 * 1024 * 1024;

use crate::bash_rewrite::footer::add_footer;
use crate::bash_rewrite::parser::parse;
use crate::bash_rewrite::RewriteRule;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};

pub struct GrepRule;
pub struct RgRule;
pub struct FindRule;
pub struct CatRule;
pub struct CatAppendRule;
pub struct SedRule;
pub struct LsRule;

impl RewriteRule for GrepRule {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn matches(&self, command: &str) -> bool {
        grep_request(command, "grep").is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = grep_request(command, "grep").ok_or("not a grep rewrite")?;
        try_call_and_footer(
            crate::commands::grep::handle_grep(&request("grep", params, session_id), ctx),
            "grep",
        )
    }
}

impl RewriteRule for RgRule {
    fn name(&self) -> &'static str {
        "rg"
    }

    fn matches(&self, command: &str) -> bool {
        grep_request(command, "rg").is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = grep_request(command, "rg").ok_or("not an rg rewrite")?;
        try_call_and_footer(
            crate::commands::grep::handle_grep(&request("grep", params, session_id), ctx),
            "grep",
        )
    }
}

impl RewriteRule for FindRule {
    fn name(&self) -> &'static str {
        "find"
    }

    fn matches(&self, command: &str) -> bool {
        find_request(command).is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = find_request(command).ok_or("not a find rewrite")?;
        try_call_and_footer(
            crate::commands::glob::handle_glob(&request("glob", params, session_id), ctx),
            "glob",
        )
    }
}

impl RewriteRule for CatRule {
    fn name(&self) -> &'static str {
        "cat"
    }

    fn matches(&self, command: &str) -> bool {
        cat_read_request(command).is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = cat_read_request(command).ok_or("not a cat rewrite")?;
        try_call_and_footer(
            crate::commands::read::handle_read(&request("read", params, session_id), ctx),
            "read",
        )
    }
}

impl RewriteRule for CatAppendRule {
    fn name(&self) -> &'static str {
        "cat_append"
    }

    fn matches(&self, command: &str) -> bool {
        append_request(command).is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = append_request(command).ok_or("not an append rewrite")?;
        try_call_and_footer(
            crate::commands::edit_match::handle_edit_match(
                &request("edit_match", params, session_id),
                ctx,
            ),
            "edit",
        )
    }
}

impl RewriteRule for SedRule {
    fn name(&self) -> &'static str {
        "sed"
    }

    fn matches(&self, command: &str) -> bool {
        sed_request(command).is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = sed_request(command).ok_or("not a sed rewrite")?;
        try_call_and_footer(
            crate::commands::read::handle_read(&request("read", params, session_id), ctx),
            "read",
        )
    }
}

impl RewriteRule for LsRule {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn matches(&self, command: &str) -> bool {
        ls_request(command).is_some()
    }

    fn rewrite(
        &self,
        command: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> Result<Response, String> {
        let params = ls_request(command).ok_or("not an ls rewrite")?;
        try_call_and_footer(
            crate::commands::read::handle_read(&request("read", params, session_id), ctx),
            "read",
        )
    }
}

fn request(command: &str, params: Value, session_id: Option<&str>) -> RawRequest {
    RawRequest {
        id: "bash_rewrite".to_string(),
        command: command.to_string(),
        lsp_hints: None,
        session_id: session_id.map(str::to_string),
        params,
    }
}

/// Run an underlying tool through the rewrite path. If the tool returned
/// `success: false`, propagate as Err so dispatch falls through to actual bash
/// — the agent's intent was bash, the rewrite is a transparent optimization.
/// Returning a wrapped error response would surprise the agent (e.g. read's
/// `outside project root` rejecting a sed that bash would have allowed).
fn try_call_and_footer(response: Response, replacement_tool: &str) -> Result<Response, String> {
    if !response.success {
        let message = response
            .data
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| response.data.get("code").and_then(Value::as_str))
            .unwrap_or("error");
        return Err(format!("{} declined: {}", replacement_tool, message));
    }
    Ok(call_and_footer(response, replacement_tool))
}

fn call_and_footer(mut response: Response, replacement_tool: &str) -> Response {
    let output = response_output(&response.data);
    let output = add_footer(&output, replacement_tool);

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

fn grep_request(command: &str, binary: &str) -> Option<Value> {
    let parsed = parse(command)?;
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
    let path = parsed.args.get(index + 1).cloned();
    if parsed.args.len() > index + 2 {
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
        "max_results": 100,
    });
    if let Some(path) = path {
        params["path"] = json!(path);
    }
    Some(params)
}

fn find_request(command: &str) -> Option<Value> {
    let parsed = parse(command)?;
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
    let pattern = format!("**/{name}");
    if path == "." {
        Some(json!({ "pattern": pattern }))
    } else {
        Some(json!({ "path": path.trim_end_matches('/'), "pattern": pattern }))
    }
}

fn cat_read_request(command: &str) -> Option<Value> {
    let parsed = parse(command)?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() {
        return None;
    }
    if parsed.args.len() != 2 || parsed.args.first()? != "cat" {
        return None;
    }
    Some(json!({ "file": parsed.args[1] }))
}

fn append_request(command: &str) -> Option<Value> {
    let parsed = parse(command)?;
    let file = parsed.appends_to.clone()?;

    let append_content = if parsed.args == ["cat"] {
        parsed.heredoc?
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

fn sed_request(command: &str) -> Option<Value> {
    let parsed = parse(command)?;
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

fn ls_request(command: &str) -> Option<Value> {
    let parsed = parse(command)?;
    if parsed.appends_to.is_some() || parsed.heredoc.is_some() || parsed.args.first()? != "ls" {
        return None;
    }

    let mut path = None;
    for arg in parsed.args.iter().skip(1) {
        if let Some(flags) = arg.strip_prefix('-') {
            if flags.is_empty() {
                return None;
            }
            for flag in flags.chars() {
                match flag {
                    // -R: recursive listing — `read` of a directory is
                    // single-level only, but the result is still a useful
                    // approximation of "what's in this tree".
                    // -a: show hidden files — `read` of a directory already
                    // includes hidden files via fs::read_dir(), so this is
                    // a no-op compared to plain `ls`.
                    'R' | 'a' => {}
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
    if let Ok(metadata) = std::fs::metadata(&target) {
        if !metadata.is_dir() {
            return None;
        }
    }
    // Path doesn't exist (yet)? Let bash handle the error itself — its
    // wording is well-known to agents and we don't gain anything by
    // rewriting a guaranteed-failing rewrite call.
    else if path.is_some() {
        return None;
    }

    Some(json!({ "file": target }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::find_request;

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
            find_request(r#"find ./src -name "*.go""#),
            Some(json!({ "path": "./src", "pattern": "**/*.go" }))
        );
    }

    #[test]
    fn find_trims_trailing_slash_from_path_arg() {
        assert_eq!(
            find_request(r#"find /tmp/foo/ -name "*.ts""#),
            Some(json!({ "path": "/tmp/foo", "pattern": "**/*.ts" }))
        );
    }
}
