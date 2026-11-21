//! Native code-search steering, kept separate from result/runner delivery.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HINT_PREFIX: &str = "DO NOT search code by running grep/rg in ";

/// Split a bash reply into its body and the closing lines AFT adds after the
/// command output: an optional `[task <id>]` line followed by `trailer` when
/// one is present. The tail is returned with its own line breaks.
pub(crate) fn split_reply_tail<'a>(output: &'a str, trailer: Option<&str>) -> (&'a str, &'a str) {
    let mut tail_start = output.len();
    if let Some(trailer) = trailer.filter(|trailer| !trailer.is_empty()) {
        if output.ends_with(trailer) {
            tail_start -= trailer.len();
        }
    }
    let before = output[..tail_start].trim_end_matches('\n');
    let last_line_start = before.rfind('\n').map_or(0, |index| index + 1);
    if crate::list_surfaces::bash::is_foreground_task_line(&before[last_line_start..]) {
        tail_start = last_line_start;
    }
    (&output[..tail_start], &output[tail_start..])
}

pub(crate) fn is_terminal_native_reply(response: &crate::protocol::Response) -> bool {
    response.data.get("task_id").is_some()
        && response.data.get("exit_code").is_some()
        && matches!(
            response.data["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
        )
}

/// Standalone hosts carry the steer in the result. Runner routes instead send
/// the decision through session.send and must not call this delivery helper.
pub(crate) fn append_nudge(response: &mut crate::protocol::Response, nudge: &str) {
    let trailer = response
        .data
        .get(crate::list_surfaces::bash::WIRE_KEY)
        .and_then(|value| {
            serde_json::from_value::<crate::list_envelope::ListEnvelope>(value.clone()).ok()
        })
        .map(|envelope| crate::list_surfaces::bash::envelope_trailer(&envelope));
    if let Some(text) = response.data["output"].as_str() {
        let (body, tail) = split_reply_tail(text, trailer.as_deref());
        let combined = format!(
            "{}\n\n{nudge}{}{tail}",
            body.trim_end_matches('\n'),
            if tail.is_empty() { "" } else { "\n" }
        );
        response.data["output"] = serde_json::Value::String(combined);
    }
}

/// Decide whether a completed native shell call gets a nudge. Launch/status
/// replies are excluded by callers; rewritten output already carries its steer.
pub(crate) fn code_search_nudge(
    output: &str,
    command: &str,
    project_root: Option<&Path>,
    workdir: Option<&str>,
    search_tool: Option<&str>,
    grep_tool: Option<&str>,
    shell_tool: &str,
) -> Option<String> {
    code_search_decision(
        output,
        command,
        project_root,
        workdir,
        search_tool,
        grep_tool,
        shell_tool,
    )
    .ok()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rejection {
    NotCodeSearch,
    Ineligible(&'static str),
}

pub(crate) fn code_search_decision(
    output: &str,
    command: &str,
    project_root: Option<&Path>,
    workdir: Option<&str>,
    search_tool: Option<&str>,
    grep_tool: Option<&str>,
    shell_tool: &str,
) -> Result<String, Rejection> {
    use Rejection::{Ineligible, NotCodeSearch};
    let windows = cfg!(windows);
    let Some(statements) = statements(command, windows) else {
        return Err(if search_stage(command, windows).is_some() {
            Ineligible("command could not be parsed")
        } else {
            NotCodeSearch
        });
    };
    let pipelines: Vec<_> = statements
        .iter()
        .filter_map(|s| pipeline_stages(s, windows))
        .collect();
    let stages = || {
        pipelines
            .iter()
            .filter_map(|pipeline| pipeline.first().copied())
    };
    let searches: Vec<_> = stages()
        .filter_map(|s| search_stage(s, windows).map(|end| operands(s, end, windows)))
        .collect();
    if searches.is_empty() {
        if pipelines.iter().any(|pipeline| {
            pipeline
                .iter()
                .skip(1)
                .any(|stage| search_stage(stage, windows).is_some())
        }) {
            return Err(Ineligible("search is not the first pipeline stage"));
        }
        return Err(
            if statements
                .iter()
                .any(|s| search_stage(s, windows).is_some())
            {
                Ineligible("search stage could not be parsed")
            } else {
                NotCodeSearch
            },
        );
    }
    if output.is_empty() {
        return Err(Ineligible("empty output"));
    }
    if output.contains(HINT_PREFIX) {
        return Err(Ineligible("output already contains the nudge"));
    }
    if search_tool.is_none() && grep_tool.is_none() {
        return Err(Ineligible("no replacement search tool"));
    }
    if searches
        .iter()
        .all(|paths| !paths.is_empty() && paths.iter().all(|p| dynamic(p)))
    {
        return Err(Ineligible("search paths are dynamic"));
    }
    if let Some(root) = project_root.filter(|root| !root.to_string_lossy().trim().is_empty()) {
        let root = root.to_str().map(str::trim).map(Path::new).unwrap_or(root);
        let root = resolve(Path::new(""), root);
        let mut cwd = Some(resolve(&root, Path::new(workdir.unwrap_or("."))));
        let mut suppress = true;
        let mut reason = "no eligible project search target";
        for stage in stages() {
            let Some((head, end)) = token(stage, 0, windows) else {
                continue;
            };
            if head == "cd" {
                cwd = next_cwd(cwd.as_deref(), stage, end, windows);
                continue;
            }
            if !matches!(head.as_str(), "grep" | "rg") {
                continue;
            }
            let Some(cwd) = cwd.as_deref() else {
                reason = "search working directory is unknown";
                continue;
            };
            if !inside(&root, cwd) {
                reason = "search working directory is outside the project";
                continue;
            }
            let paths = operands(stage, end, windows);
            if paths.is_empty() {
                suppress = false;
                break;
            }
            let mut existing = false;
            for path in paths {
                if dynamic(&path) {
                    reason = "search path is dynamic";
                    continue;
                }
                let path = resolve(cwd, Path::new(&path));
                if !inside(&root, &path) {
                    reason = "search path is outside the project";
                    continue;
                }
                let Ok(metadata) = std::fs::metadata(path) else {
                    reason = "search target metadata is unavailable";
                    continue;
                };
                existing = true;
                if metadata.is_file() {
                    return Err(Ineligible("search targets a file"));
                }
                if metadata.modified().is_ok_and(fresh) {
                    return Err(Ineligible("search target was recently modified"));
                }
            }
            if existing {
                suppress = false;
                break;
            }
        }
        if suppress {
            return Err(Ineligible(reason));
        }
    }
    Ok(crate::bash_rewrite::footer::grep_search_hint(
        search_tool,
        grep_tool.unwrap_or("grep"),
        shell_tool,
    ))
}

fn search_stage(stage: &str, windows: bool) -> Option<usize> {
    let (head, end) = token(stage, 0, windows)?;
    matches!(head.as_str(), "grep" | "rg").then_some(end)
}

fn fresh(modified: SystemTime) -> bool {
    fresh_at(modified, SystemTime::now())
}

pub(crate) fn fresh_at(modified: SystemTime, now: SystemTime) -> bool {
    // Match Date.now()'s millisecond precision, including sub-ms mtime slack.
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let modified_ms = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0;
    let age = now_ms - modified_ms;
    age > -1.0 && age < 60_000.0
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir().unwrap_or_default().join(joined)
    };
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn inside(root: &Path, path: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|r| {
        path.next().is_some_and(|p| {
            if cfg!(windows) {
                r.as_os_str().to_string_lossy().to_lowercase()
                    == p.as_os_str().to_string_lossy().to_lowercase()
            } else {
                r == p
            }
        })
    })
}

fn next_cwd(cwd: Option<&Path>, stage: &str, mut at: usize, windows: bool) -> Option<PathBuf> {
    loop {
        at = skip_spaces(stage, at);
        if at == stage.len() {
            break;
        }
        let Some((part, end)) = token(stage, at, windows) else {
            break;
        };
        if end <= at {
            break;
        }
        at = end;
        if part == "-" {
            return None;
        }
        if part.starts_with('-') {
            continue;
        }
        if part.contains('$') || part.contains('`') {
            return None;
        }
        let expanded = if part == "~" || part.starts_with("~/") {
            std::env::home_dir()?.join(part.trim_start_matches('~').trim_start_matches('/'))
        } else {
            PathBuf::from(part)
        };
        return if expanded.is_absolute() {
            Some(resolve(Path::new(""), &expanded))
        } else {
            Some(resolve(cwd?, &expanded))
        };
    }
    std::env::home_dir().map(|home| resolve(Path::new(""), &home))
}

fn dynamic(path: &str) -> bool {
    path.starts_with('~') || path.contains('$') || path.contains('`')
}

pub(crate) fn operands(stage: &str, mut at: usize, windows: bool) -> Vec<String> {
    let mut paths = Vec::new();
    let mut pattern = false;
    let mut options_ended = false;
    let mut option_value = None;
    loop {
        at = skip_spaces(stage, at);
        if at == stage.len() {
            break;
        }
        let Some((part, end)) = token(stage, at, windows) else {
            break;
        };
        if end <= at {
            break;
        }
        at = end;
        if part.chars().all(|c| c.is_ascii_digit()) && stage[end..].starts_with(['<', '>']) {
            break;
        }
        if let Some(is_pattern) = option_value.take() {
            if is_pattern {
                pattern = true;
            }
            continue;
        }
        if !options_ended && part == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && part.starts_with('-') {
            if matches!(part.as_str(), "-e" | "--regexp" | "-f" | "--file") {
                option_value = Some(true);
            } else if part.starts_with("--regexp=")
                || part.starts_with("--file=")
                || (part.len() > 2 && (part.starts_with("-e") || part.starts_with("-f")))
            {
                pattern = true;
            } else if [
                "-A",
                "-B",
                "-C",
                "-m",
                "--after-context",
                "--before-context",
                "--context",
                "--max-count",
                "--include",
                "--exclude",
                "--exclude-dir",
                "-g",
                "--glob",
                "-t",
                "--type",
                "-T",
                "--type-not",
            ]
            .contains(&part.as_str())
            {
                option_value = Some(false);
            }
            continue;
        }
        if !pattern {
            pattern = true;
        } else {
            paths.push(part);
        }
    }
    paths
}

fn skip_spaces(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(input.len(), |(at, _)| start + at)
}

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

fn escapes(next: Option<char>, quote: Quote, windows: bool) -> bool {
    if quote == Quote::Double {
        next.is_some_and(|c| "$`\"\\\n".contains(c))
    } else {
        !windows
    }
}

pub(crate) fn statements(command: &str, windows: bool) -> Option<Vec<&str>> {
    let chars: Vec<_> = command.char_indices().collect();
    let mut out = Vec::new();
    let (mut start, mut i, mut depth) = (0, 0, 0usize);
    let mut quote = Quote::None;
    let (mut escaped, mut backtick) = (false, false);
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|(_, c)| *c);
        i += 1;
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            }
            continue;
        }
        if quote == Quote::Double {
            if c == '\\' && escapes(next, quote, windows) {
                escaped = true;
            } else if c == '"' {
                quote = Quote::None;
            }
            continue;
        }
        if backtick {
            if c == '`' {
                backtick = false;
            }
            continue;
        }
        if c == '\\' && escapes(next, quote, windows) {
            escaped = true;
            continue;
        }
        match c {
            '\'' => {
                quote = Quote::Single;
                continue;
            }
            '"' => {
                quote = Quote::Double;
                continue;
            }
            '`' => {
                backtick = true;
                continue;
            }
            '(' => {
                depth += 1;
                continue;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ => {}
        }
        if depth > 0 {
            continue;
        }
        if (c == '&' && next == Some('&')) || (c == '|' && next == Some('|')) {
            out.push(&command[start..at]);
            i += 1;
            start = at + 2;
        } else if matches!(c, ';' | '\n' | '&') {
            out.push(&command[start..at]);
            start = at + 1;
        }
    }
    if quote != Quote::None || backtick || escaped {
        return None;
    }
    out.push(&command[start..]);
    Some(out)
}

pub(crate) fn pipeline_stages(command: &str, windows: bool) -> Option<Vec<&str>> {
    let mut chars = command.char_indices().peekable();
    let mut quote = Quote::None;
    let mut stages = Vec::new();
    let mut start = 0;
    while let Some((at, c)) = chars.next() {
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            }
            continue;
        }
        if c == '\\' && escapes(chars.peek().map(|(_, c)| *c), quote, windows) {
            chars.next();
            continue;
        }
        if c == '`' {
            return None;
        }
        if quote == Quote::Double {
            if c == '"' {
                quote = Quote::None;
            }
            continue;
        }
        match c {
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            '|' if chars.peek().is_some_and(|(_, c)| *c == '|') => {
                chars.next();
            }
            '|' => {
                stages.push(command[start..at].trim());
                start = at + 1;
            }
            _ => {}
        }
    }
    if quote != Quote::None {
        return None;
    }
    stages.push(command[start..].trim());
    Some(stages)
}

pub(crate) fn token(command: &str, start: usize, windows: bool) -> Option<(String, usize)> {
    let mut chars = command[start..].char_indices().peekable();
    while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        chars.next();
    }
    let mut out = String::new();
    let mut quote = Quote::None;
    while let Some((at, c)) = chars.next() {
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            } else {
                out.push(c);
            }
            continue;
        }
        if c == '\\' && escapes(chars.peek().map(|(_, c)| *c), quote, windows) {
            if let Some((_, c)) = chars.next() {
                if quote != Quote::Double || c != '\n' {
                    out.push(c);
                }
            } else {
                out.push('\\');
            }
            continue;
        }
        if c == '`' {
            return None;
        }
        if quote == Quote::Double {
            if c == '"' {
                quote = Quote::None;
            } else {
                out.push(c);
            }
            continue;
        }
        if c.is_whitespace() || "|;&<>".contains(c) {
            return Some((out, start + at));
        }
        match c {
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            _ => out.push(c),
        }
    }
    (quote == Quote::None).then_some((out, command.len()))
}
